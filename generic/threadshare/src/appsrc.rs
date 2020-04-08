// Copyright (C) 2018 Sebastian Dröge <sebastian@centricular.com>
// Copyright (C) 2019-2020 François Laignel <fengalin@free.fr>
//
// This library is free software; you can redistribute it and/or
// modify it under the terms of the GNU Library General Public
// License as published by the Free Software Foundation; either
// version 2 of the License, or (at your option) any later version.
//
// This library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
// Library General Public License for more details.
//
// You should have received a copy of the GNU Library General Public
// License along with this library; if not, write to the
// Free Software Foundation, Inc., 51 Franklin Street, Suite 500,
// Boston, MA 02110-1335, USA.

use futures::channel::mpsc;
use futures::lock::Mutex as FutMutex;
use futures::prelude::*;

use glib;
use glib::prelude::*;
use glib::subclass;
use glib::subclass::prelude::*;
use glib::{glib_object_impl, glib_object_subclass};

use gst;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_element_error, gst_error, gst_error_msg, gst_log, gst_trace};

use lazy_static::lazy_static;

use rand;

use std::convert::TryInto;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::u32;

use crate::runtime::prelude::*;
use crate::runtime::{Context, PadSrc, PadSrcRef, Task};

const DEFAULT_CONTEXT: &str = "";
const DEFAULT_CONTEXT_WAIT: u32 = 0;
const DEFAULT_CAPS: Option<gst::Caps> = None;
const DEFAULT_MAX_BUFFERS: u32 = 10;
const DEFAULT_DO_TIMESTAMP: bool = false;

#[derive(Debug, Clone)]
struct Settings {
    context: String,
    context_wait: u32,
    caps: Option<gst::Caps>,
    max_buffers: u32,
    do_timestamp: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            context: DEFAULT_CONTEXT.into(),
            context_wait: DEFAULT_CONTEXT_WAIT,
            caps: DEFAULT_CAPS,
            max_buffers: DEFAULT_MAX_BUFFERS,
            do_timestamp: DEFAULT_DO_TIMESTAMP,
        }
    }
}

static PROPERTIES: [subclass::Property; 5] = [
    subclass::Property("context", |name| {
        glib::ParamSpec::string(
            name,
            "Context",
            "Context name to share threads with",
            Some(DEFAULT_CONTEXT),
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("context-wait", |name| {
        glib::ParamSpec::uint(
            name,
            "Context Wait",
            "Throttle poll loop to run at most once every this many ms",
            0,
            1000,
            DEFAULT_CONTEXT_WAIT,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("max-buffers", |name| {
        glib::ParamSpec::uint(
            name,
            "Max Buffers",
            "Maximum number of buffers to queue up",
            1,
            u32::MAX,
            DEFAULT_MAX_BUFFERS,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("caps", |name| {
        glib::ParamSpec::boxed(
            name,
            "Caps",
            "Caps to use",
            gst::Caps::static_type(),
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("do-timestamp", |name| {
        glib::ParamSpec::boolean(
            name,
            "Do Timestamp",
            "Timestamp buffers with the current running time on arrival",
            DEFAULT_DO_TIMESTAMP,
            glib::ParamFlags::READWRITE,
        )
    }),
];

lazy_static! {
    static ref CAT: gst::DebugCategory = gst::DebugCategory::new(
        "ts-appsrc",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing app source"),
    );
}

#[derive(Debug)]
enum StreamItem {
    Buffer(gst::Buffer),
    Event(gst::Event),
}

#[derive(Debug)]
struct AppSrcPadHandlerState {
    need_initial_events: bool,
    need_segment: bool,
    caps: Option<gst::Caps>,
}

impl Default for AppSrcPadHandlerState {
    fn default() -> Self {
        AppSrcPadHandlerState {
            need_initial_events: true,
            need_segment: true,
            caps: None,
        }
    }
}

#[derive(Debug, Default)]
struct AppSrcPadHandlerInner {
    state: FutMutex<AppSrcPadHandlerState>,
    configured_caps: StdMutex<Option<gst::Caps>>,
}

#[derive(Clone, Debug, Default)]
struct AppSrcPadHandler(Arc<AppSrcPadHandlerInner>);

impl AppSrcPadHandler {
    fn prepare(&self, caps: Option<gst::Caps>) {
        self.0
            .state
            .try_lock()
            .expect("State locked elsewhere")
            .caps = caps;
    }

    fn reset(&self) {
        *self.0.state.try_lock().expect("State locked elsewhere") = Default::default();
        *self.0.configured_caps.lock().unwrap() = None;
    }

    fn set_need_segment(&self) {
        self.0
            .state
            .try_lock()
            .expect("State locked elsewhere")
            .need_segment = true;
    }

    async fn push_prelude(&self, pad: &PadSrcRef<'_>, _element: &gst::Element) {
        let mut state = self.0.state.lock().await;
        if state.need_initial_events {
            gst_debug!(CAT, obj: pad.gst_pad(), "Pushing initial events");

            let stream_id = format!("{:08x}{:08x}", rand::random::<u32>(), rand::random::<u32>());
            let stream_start_evt = gst::Event::new_stream_start(&stream_id)
                .group_id(gst::GroupId::next())
                .build();
            pad.push_event(stream_start_evt).await;

            if let Some(ref caps) = state.caps {
                let caps_evt = gst::Event::new_caps(&caps).build();
                pad.push_event(caps_evt).await;
                *self.0.configured_caps.lock().unwrap() = Some(caps.clone());
            }

            state.need_initial_events = false;
        }

        if state.need_segment {
            let segment_evt =
                gst::Event::new_segment(&gst::FormattedSegment::<gst::format::Time>::new()).build();
            pad.push_event(segment_evt).await;

            state.need_segment = false;
        }
    }

    async fn push_item(
        self,
        pad: &PadSrcRef<'_>,
        element: &gst::Element,
        item: StreamItem,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_log!(CAT, obj: pad.gst_pad(), "Handling {:?}", item);

        self.push_prelude(pad, element).await;

        match item {
            StreamItem::Buffer(buffer) => {
                gst_log!(CAT, obj: pad.gst_pad(), "Forwarding {:?}", buffer);
                pad.push(buffer).await
            }
            StreamItem::Event(event) => {
                gst_log!(CAT, obj: pad.gst_pad(), "Forwarding {:?}", event);
                pad.push_event(event).await;
                Ok(gst::FlowSuccess::Ok)
            }
        }
    }
}

impl PadSrcHandler for AppSrcPadHandler {
    type ElementImpl = AppSrc;

    fn src_event(
        &self,
        pad: &PadSrcRef,
        appsrc: &AppSrc,
        element: &gst::Element,
        event: gst::Event,
    ) -> bool {
        use gst::EventView;

        gst_log!(CAT, obj: pad.gst_pad(), "Handling {:?}", event);

        let ret = match event.view() {
            EventView::FlushStart(..) => {
                appsrc.flush_start(element);
                true
            }
            EventView::FlushStop(..) => {
                appsrc.flush_stop(element);
                true
            }
            EventView::Reconfigure(..) => true,
            EventView::Latency(..) => true,
            _ => false,
        };

        if ret {
            gst_log!(CAT, obj: pad.gst_pad(), "Handled {:?}", event);
        } else {
            gst_log!(CAT, obj: pad.gst_pad(), "Didn't handle {:?}", event);
        }

        ret
    }

    fn src_query(
        &self,
        pad: &PadSrcRef,
        _appsrc: &AppSrc,
        _element: &gst::Element,
        query: &mut gst::QueryRef,
    ) -> bool {
        use gst::QueryView;

        gst_log!(CAT, obj: pad.gst_pad(), "Handling {:?}", query);
        let ret = match query.view_mut() {
            QueryView::Latency(ref mut q) => {
                q.set(true, 0.into(), gst::CLOCK_TIME_NONE);
                true
            }
            QueryView::Scheduling(ref mut q) => {
                q.set(gst::SchedulingFlags::SEQUENTIAL, 1, -1, 0);
                q.add_scheduling_modes(&[gst::PadMode::Push]);
                true
            }
            QueryView::Caps(ref mut q) => {
                let caps = if let Some(caps) = self.0.configured_caps.lock().unwrap().as_ref() {
                    q.get_filter()
                        .map(|f| f.intersect_with_mode(caps, gst::CapsIntersectMode::First))
                        .unwrap_or_else(|| caps.clone())
                } else {
                    q.get_filter()
                        .map(|f| f.to_owned())
                        .unwrap_or_else(gst::Caps::new_any)
                };

                q.set_result(&caps);

                true
            }
            _ => false,
        };

        if ret {
            gst_log!(CAT, obj: pad.gst_pad(), "Handled {:?}", query);
        } else {
            gst_log!(CAT, obj: pad.gst_pad(), "Didn't handle {:?}", query);
        }
        ret
    }
}

#[derive(Debug, Eq, PartialEq)]
enum AppSrcState {
    Paused,
    RejectBuffers,
    Started,
}

#[derive(Debug)]
struct AppSrc {
    src_pad: PadSrc,
    src_pad_handler: AppSrcPadHandler,
    task: Task,
    state: StdMutex<AppSrcState>,
    sender: StdMutex<Option<mpsc::Sender<StreamItem>>>,
    receiver: StdMutex<Option<Arc<FutMutex<mpsc::Receiver<StreamItem>>>>>,
    settings: StdMutex<Settings>,
}

impl AppSrc {
    fn push_buffer(&self, element: &gst::Element, mut buffer: gst::Buffer) -> bool {
        let state = self.state.lock().unwrap();
        if *state == AppSrcState::RejectBuffers {
            gst_debug!(CAT, obj: element, "Rejecting buffer due to pad state");
            return false;
        }

        let do_timestamp = self.settings.lock().unwrap().do_timestamp;
        if do_timestamp {
            if let Some(clock) = element.get_clock() {
                let base_time = element.get_base_time();
                let now = clock.get_time();

                let buffer = buffer.make_mut();
                buffer.set_dts(now - base_time);
                buffer.set_pts(gst::CLOCK_TIME_NONE);
            } else {
                gst_error!(CAT, obj: element, "Don't have a clock yet");
                return false;
            }
        }

        match self
            .sender
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .try_send(StreamItem::Buffer(buffer))
        {
            Ok(_) => true,
            Err(err) => {
                gst_error!(CAT, obj: element, "Failed to queue buffer: {}", err);
                false
            }
        }
    }

    fn end_of_stream(&self, element: &gst::Element) -> bool {
        let mut sender = self.sender.lock().unwrap();
        let sender = match sender.as_mut() {
            Some(sender) => sender,
            None => return false,
        };

        let eos = StreamItem::Event(gst::Event::new_eos().build());
        match sender.try_send(eos) {
            Ok(_) => true,
            Err(err) => {
                gst_error!(CAT, obj: element, "Failed to queue EOS: {}", err);
                false
            }
        }
    }

    fn prepare(&self, element: &gst::Element) -> Result<(), gst::ErrorMessage> {
        let settings = self.settings.lock().unwrap();
        gst_debug!(CAT, obj: element, "Preparing");

        let context =
            Context::acquire(&settings.context, settings.context_wait).map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to acquire Context: {}", err]
                )
            })?;

        let max_buffers = settings.max_buffers.try_into().map_err(|err| {
            gst_error_msg!(
                gst::ResourceError::Settings,
                ["Invalid max-buffers: {}, {}", settings.max_buffers, err]
            )
        })?;

        let (sender, receiver) = mpsc::channel(max_buffers);
        *self.sender.lock().unwrap() = Some(sender);
        *self.receiver.lock().unwrap() = Some(Arc::new(FutMutex::new(receiver)));

        self.task.prepare(context).map_err(|err| {
            gst_error_msg!(
                gst::ResourceError::OpenRead,
                ["Error preparing Task: {:?}", err]
            )
        })?;
        self.src_pad_handler.prepare(settings.caps.clone());
        self.src_pad.prepare(&self.src_pad_handler);

        gst_debug!(CAT, obj: element, "Prepared");

        Ok(())
    }

    fn unprepare(&self, element: &gst::Element) -> Result<(), ()> {
        gst_debug!(CAT, obj: element, "Unpreparing");

        *self.sender.lock().unwrap() = None;
        *self.receiver.lock().unwrap() = None;

        self.task.unprepare().unwrap();
        self.src_pad.unprepare();

        gst_debug!(CAT, obj: element, "Unprepared");

        Ok(())
    }

    fn stop(&self, element: &gst::Element) -> Result<(), ()> {
        let mut state = self.state.lock().unwrap();
        gst_debug!(CAT, obj: element, "Stopping");

        self.flush(element);
        self.src_pad_handler.reset();
        *state = AppSrcState::RejectBuffers;

        gst_debug!(CAT, obj: element, "Stopped");

        Ok(())
    }

    fn flush(&self, element: &gst::Element) {
        gst_log!(CAT, obj: element, "Flushing");

        self.task.stop();

        let receiver = self.receiver.lock().unwrap();
        let mut receiver = receiver
            .as_ref()
            .unwrap()
            .try_lock()
            .expect("receiver locked elsewhere");

        // Purge the channel
        loop {
            match receiver.try_next() {
                Ok(Some(_item)) => {
                    gst_log!(CAT, obj: element, "Dropping pending item");
                }
                Err(_) => {
                    gst_log!(CAT, obj: element, "No more pending item");
                    break;
                }
                Ok(None) => {
                    panic!("Channel sender dropped");
                }
            }
        }

        self.src_pad_handler.set_need_segment();

        gst_log!(CAT, obj: element, "Flushed");
    }

    fn start(&self, element: &gst::Element) -> Result<(), ()> {
        let mut state = self.state.lock().unwrap();
        if *state == AppSrcState::Started {
            gst_debug!(CAT, obj: element, "Already started");
            return Ok(());
        }

        gst_debug!(CAT, obj: element, "Starting");

        self.start_task(element);
        *state = AppSrcState::Started;

        gst_debug!(CAT, obj: element, "Started");

        Ok(())
    }

    fn start_task(&self, element: &gst::Element) {
        let src_pad_handler = self.src_pad_handler.clone();
        let pad_weak = self.src_pad.downgrade();
        let element = element.clone();
        let receiver = Arc::clone(self.receiver.lock().unwrap().as_ref().expect("No receiver"));

        self.task.start(move || {
            let src_pad_handler = src_pad_handler.clone();
            let pad_weak = pad_weak.clone();
            let element = element.clone();
            let receiver = Arc::clone(&receiver);

            async move {
                let item = receiver.lock().await.next().await;

                let pad = pad_weak.upgrade().expect("PadSrc no longer exists");

                let item = match item {
                    Some(item) => item,
                    None => {
                        gst_log!(CAT, obj: pad.gst_pad(), "SrcPad channel aborted");
                        return glib::Continue(false);
                    }
                };

                match src_pad_handler.push_item(&pad, &element, item).await {
                    Ok(_) => {
                        gst_log!(CAT, obj: pad.gst_pad(), "Successfully pushed item");
                        glib::Continue(true)
                    }
                    Err(gst::FlowError::Eos) => {
                        gst_debug!(CAT, obj: pad.gst_pad(), "EOS");
                        let eos = gst::Event::new_eos().build();
                        pad.push_event(eos).await;
                        glib::Continue(false)
                    }
                    Err(gst::FlowError::Flushing) => {
                        gst_debug!(CAT, obj: pad.gst_pad(), "Flushing");
                        glib::Continue(false)
                    }
                    Err(err) => {
                        gst_error!(CAT, obj: pad.gst_pad(), "Got error {}", err);
                        gst_element_error!(
                            &element,
                            gst::StreamError::Failed,
                            ("Internal data stream error"),
                            ["streaming stopped, reason {}", err]
                        );
                        glib::Continue(false)
                    }
                }
            }
        });
    }

    fn flush_stop(&self, element: &gst::Element) {
        let mut state = self.state.lock().unwrap();
        if *state == AppSrcState::Started {
            gst_debug!(CAT, obj: element, "Already started");
            return;
        }

        gst_debug!(CAT, obj: element, "Stopping Flush");

        self.flush(element);
        self.start_task(element);
        *state = AppSrcState::Started;

        gst_debug!(CAT, obj: element, "Flush Stopped");
    }

    fn flush_start(&self, element: &gst::Element) {
        let mut state = self.state.lock().unwrap();
        gst_debug!(CAT, obj: element, "Starting Flush");

        self.task.cancel();
        *state = AppSrcState::RejectBuffers;

        gst_debug!(CAT, obj: element, "Flush Started");
    }

    fn pause(&self, element: &gst::Element) -> Result<(), ()> {
        let mut state = self.state.lock().unwrap();
        gst_debug!(CAT, obj: element, "Pausing");

        self.task.pause();
        *state = AppSrcState::Paused;

        gst_debug!(CAT, obj: element, "Paused");

        Ok(())
    }
}

impl ObjectSubclass for AppSrc {
    const NAME: &'static str = "RsTsAppSrc";
    type ParentType = gst::Element;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn class_init(klass: &mut subclass::simple::ClassStruct<Self>) {
        klass.set_metadata(
            "Thread-sharing app source",
            "Source/Generic",
            "Thread-sharing app source",
            "Sebastian Dröge <sebastian@centricular.com>",
        );

        let caps = gst::Caps::new_any();

        let src_pad_template = gst::PadTemplate::new(
            "src",
            gst::PadDirection::Src,
            gst::PadPresence::Always,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(src_pad_template);

        klass.install_properties(&PROPERTIES);

        klass.add_signal_with_class_handler(
            "push-buffer",
            glib::SignalFlags::RUN_LAST | glib::SignalFlags::ACTION,
            &[gst::Buffer::static_type()],
            bool::static_type(),
            |_, args| {
                let element = args[0]
                    .get::<gst::Element>()
                    .expect("signal arg")
                    .expect("missing signal arg");
                let buffer = args[1]
                    .get::<gst::Buffer>()
                    .expect("signal arg")
                    .expect("missing signal arg");
                let appsrc = Self::from_instance(&element);

                Some(appsrc.push_buffer(&element, buffer).to_value())
            },
        );

        klass.add_signal_with_class_handler(
            "end-of-stream",
            glib::SignalFlags::RUN_LAST | glib::SignalFlags::ACTION,
            &[],
            bool::static_type(),
            |_, args| {
                let element = args[0]
                    .get::<gst::Element>()
                    .expect("signal arg")
                    .expect("missing signal arg");
                let appsrc = Self::from_instance(&element);
                Some(appsrc.end_of_stream(&element).to_value())
            },
        );
    }

    fn new_with_class(klass: &subclass::simple::ClassStruct<Self>) -> Self {
        Self {
            src_pad: PadSrc::new(gst::Pad::new_from_template(
                &klass.get_pad_template("src").unwrap(),
                Some("src"),
            )),
            src_pad_handler: AppSrcPadHandler::default(),
            task: Task::default(),
            state: StdMutex::new(AppSrcState::RejectBuffers),
            sender: StdMutex::new(None),
            receiver: StdMutex::new(None),
            settings: StdMutex::new(Settings::default()),
        }
    }
}

impl ObjectImpl for AppSrc {
    glib_object_impl!();

    fn set_property(&self, _obj: &glib::Object, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id];

        let mut settings = self.settings.lock().unwrap();
        match *prop {
            subclass::Property("context", ..) => {
                settings.context = value
                    .get()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| "".into());
            }
            subclass::Property("context-wait", ..) => {
                settings.context_wait = value.get_some().expect("type checked upstream");
            }
            subclass::Property("caps", ..) => {
                settings.caps = value.get().expect("type checked upstream");
            }
            subclass::Property("max-buffers", ..) => {
                settings.max_buffers = value.get_some().expect("type checked upstream");
            }
            subclass::Property("do-timestamp", ..) => {
                settings.do_timestamp = value.get_some().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn get_property(&self, _obj: &glib::Object, id: usize) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id];

        let settings = self.settings.lock().unwrap();
        match *prop {
            subclass::Property("context", ..) => Ok(settings.context.to_value()),
            subclass::Property("context-wait", ..) => Ok(settings.context_wait.to_value()),
            subclass::Property("caps", ..) => Ok(settings.caps.to_value()),
            subclass::Property("max-buffers", ..) => Ok(settings.max_buffers.to_value()),
            subclass::Property("do-timestamp", ..) => Ok(settings.do_timestamp.to_value()),
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &glib::Object) {
        self.parent_constructed(obj);

        let element = obj.downcast_ref::<gst::Element>().unwrap();
        element.add_pad(self.src_pad.gst_pad()).unwrap();

        super::set_element_flags(element, gst::ElementFlags::SOURCE);
    }
}

impl ElementImpl for AppSrc {
    fn change_state(
        &self,
        element: &gst::Element,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_trace!(CAT, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::NullToReady => {
                self.prepare(element).map_err(|err| {
                    element.post_error_message(&err);
                    gst::StateChangeError
                })?;
            }
            gst::StateChange::PlayingToPaused => {
                self.pause(element).map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::ReadyToNull => {
                self.unprepare(element).map_err(|_| gst::StateChangeError)?;
            }
            _ => (),
        }

        let mut success = self.parent_change_state(element, transition)?;

        match transition {
            gst::StateChange::ReadyToPaused => {
                success = gst::StateChangeSuccess::NoPreroll;
            }
            gst::StateChange::PausedToPlaying => {
                self.start(element).map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::PlayingToPaused => {
                success = gst::StateChangeSuccess::NoPreroll;
            }
            gst::StateChange::PausedToReady => {
                self.stop(element).map_err(|_| gst::StateChangeError)?;
            }
            _ => (),
        }

        Ok(success)
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "ts-appsrc",
        gst::Rank::None,
        AppSrc::get_type(),
    )
}