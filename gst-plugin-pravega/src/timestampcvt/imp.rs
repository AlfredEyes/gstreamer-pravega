//
// Copyright (c) Dell Inc., or its subsidiaries. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//

use gst::ClockTime;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
#[allow(unused_imports)]
use gst::{debug, error, warning, info, log, trace};
use once_cell::sync::Lazy;
use pravega_video::timestamp::{PravegaTimestamp, MSECOND};
use std::convert::TryFrom;
use std::sync::Mutex;
use crate::utils::pravega_to_clocktime;

pub const ELEMENT_NAME: &str = "timestampcvt";
const ELEMENT_CLASS_NAME: &str = "TimestampCvt";
const ELEMENT_LONG_NAME: &str = "Convert timestamps";
const ELEMENT_DESCRIPTION: &str = "\
This element converts PTS and DTS timestamps for buffers.\
Use this for pipelines that will eventually write to pravegasink (timestamp-mode=tai). \
This element drops any buffers without PTS. \
Additionally, any PTS values that decrease will have their PTS corrected.";
const ELEMENT_AUTHOR: &str = "Claudio Fahey <claudio.fahey@dell.com>";
const DEBUG_CATEGORY: &str = ELEMENT_NAME;

const PROPERTY_NAME_INPUT_TIMESTAMP_MODE: &str = "input-timestamp-mode";
const PROPERTY_NAME_START_UTC: &str = "start-utc";
const NICK_START_AT_CURRENT_TIME: &str = "start-at-current-time";

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstInputTimestampMode")]
pub enum InputTimestampMode {
    #[enum_value(
        name = "Input buffer timestamps are nanoseconds \
                since the NTP epoch 1900-01-01 00:00:00 UTC, not including leap seconds. \
                Use this for buffers from rtspsrc (ntp-sync=true ntp-time-source=running-time) \
                with an RTSP camera that sends RTCP Sender Reports.",
        nick = "ntp"
    )]
    Ntp = 0,

    #[enum_value(
        name = "Input buffer timestamps are nanoseconds \
                since 1970-01-01 00:00:00 TAI International Atomic Time, including leap seconds. \
                Use this for buffers from pravegasrc.",
        nick = "tai"
    )]
    Tai = 1,

    #[enum_value(
        name = "The first buffer corresponds with the current time. \
                All output buffer timestamps will be offset by the same amount.",
        nick = "start-at-current-time"
    )]
    StartAtCurrentTime = 2,

    #[enum_value(
        name = "The first buffer corresponds to the fixed time specified in start-utc. \
                All buffer timestamps will be offset by the same amount.",
        nick = "start-at-fixed-time"
    )]
    StartAtFixedTime = 3,
}

const DEFAULT_INPUT_TIMESTAMP_MODE: InputTimestampMode = InputTimestampMode::Ntp;
const DEFAULT_START_TIMESTAMP: u64 = 0;

#[derive(Debug)]
struct Settings {
    input_timestamp_mode: InputTimestampMode,
    start_timestamp: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            input_timestamp_mode: DEFAULT_INPUT_TIMESTAMP_MODE,
            start_timestamp: DEFAULT_START_TIMESTAMP,
        }
    }
}

#[derive(Debug)]
struct StartedState {
    prev_input_pts: Option<ClockTime>,
    prev_output_pts: PravegaTimestamp,
    pts_offset_nanos: Option<i128>,
}

enum State {
    Started {
        state: StartedState,
    }
}

impl Default for State {
    fn default() -> State {
        State::Started {
            state: StartedState {
                prev_input_pts: ClockTime::NONE,
                prev_output_pts: PravegaTimestamp::none(),
                pts_offset_nanos: None,
            }
        }
    }
}

pub struct TimestampCvt {
    settings: Mutex<Settings>,
    state: Mutex<State>,
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
}

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        DEBUG_CATEGORY,
        gst::DebugColorFlags::empty(),
        Some(ELEMENT_LONG_NAME),
    )
});

impl TimestampCvt {
    fn sink_chain(
        &self,
        pad: &gst::Pad,
        mut buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {

        let (input_timestamp_mode, start_timestamp) = {
            let settings = self.settings.lock().unwrap();
            (settings.input_timestamp_mode, settings.start_timestamp)
        };

        let mut state = self.state.lock().unwrap();
        let state = match *state {
            State::Started { ref mut state } => state,
        };

        // If input PTS decreases, the output PTS will be set to the previous PTS plus this amount.
        let pts_correction_delta = 15 * MSECOND;

        let input_pts = buffer.pts();
        let input_dts = buffer.dts();
        if input_pts.is_some() {
            let input_nanos = input_pts.unwrap().nseconds();
            // corrected_input_pts will be the TAI timestamp of the input buffer.
            let corrected_input_pts = match input_timestamp_mode {
                InputTimestampMode::Tai => {
                    input_pts
                },
                InputTimestampMode::Ntp => {
                    pravega_to_clocktime(PravegaTimestamp::from_ntp_nanoseconds(input_pts.map(ClockTime::nseconds)))
                },
                InputTimestampMode::StartAtCurrentTime => {
                    if state.pts_offset_nanos.is_none() {
                        let now = PravegaTimestamp::now();
                        state.pts_offset_nanos = Some(now.nanoseconds().unwrap() as i128 - input_nanos as i128);
                        info!(CAT, obj: pad,
                            "Input buffer PTS timestamps will be adjusted by {} nanoseconds to synchronize with the current system time.",
                            state.pts_offset_nanos.unwrap());
                        }
                    Some(ClockTime::from_nseconds((input_nanos as i128 + state.pts_offset_nanos.unwrap()) as u64))
                },
                InputTimestampMode::StartAtFixedTime => {
                    if state.pts_offset_nanos.is_none() {
                        state.pts_offset_nanos = Some(start_timestamp as i128 - input_nanos as i128);
                        info!(CAT, obj: pad,
                            "Input buffer PTS timestamps will be adjusted by {} nanoseconds.",
                            state.pts_offset_nanos.unwrap());
                        }
                    Some(ClockTime::from_nseconds((input_nanos as i128 + state.pts_offset_nanos.unwrap()) as u64))
                },
            };
            let output_pts = if state.prev_input_pts.is_some() {
                if state.prev_input_pts == corrected_input_pts {
                    // PTS has not changed.
                    state.prev_output_pts
                } else {
                    // PTS has changed. Calculate new output PTS.
                    let output_pts = PravegaTimestamp::from_nanoseconds(corrected_input_pts.map(ClockTime::nseconds));
                    if state.prev_output_pts < output_pts {
                        // PTS has increased normally.
                        output_pts
                    } else {
                        // Output PTS has decreased.
                        let time_delta = state.prev_output_pts - output_pts;
                        let corrected_pts = state.prev_output_pts + pts_correction_delta;
                        warning!(CAT, obj: pad, "Output PTS would have decreased by {} from {} to {}. Correcting PTS to {}.",
                            time_delta, state.prev_output_pts, output_pts, corrected_pts);
                        corrected_pts
                    }
                }
            } else {
                // This is our first buffer with a PTS.
                PravegaTimestamp::from_nanoseconds(corrected_input_pts.map(ClockTime::nseconds))
            };
            let success = if output_pts.is_some() {
                if state.prev_output_pts.is_some() && output_pts < state.prev_output_pts {
                    error!(CAT, obj: pad, "Internal error. prev_output_pts={}, output_pts={}",
                        state.prev_output_pts, output_pts);
                    Err(gst::FlowError::Error)
                } else {
                    state.prev_input_pts = corrected_input_pts;
                    state.prev_output_pts = output_pts;
                    let output_pts_clocktime = pravega_to_clocktime(output_pts);
                    let buffer_ref = buffer.make_mut();
                    log!(CAT, obj: pad, "Input PTS {}, Output PTS {:?}", input_pts.unwrap_or_default(), output_pts);
                    buffer_ref.set_pts(output_pts_clocktime);

                    // Adjust DTS if it exists by the nominal PTS offset.
                    if input_dts.is_some() && state.pts_offset_nanos.is_some() {
                        let output_dts = ClockTime::from_nseconds((input_dts.unwrap().nseconds() as i128 + state.pts_offset_nanos.unwrap()) as u64);
                        log!(CAT, obj: pad, "Input DTS {}, Output DTS {:?}", input_dts.unwrap_or_default(), output_dts);
                        buffer_ref.set_dts(output_dts);
                    }

                    self.srcpad.push(buffer)
                }
            } else {
                // For some RTSP sources, buffers during the first 5 seconds will have PTS near 0.
                // This will be logged as a warning.
                // If this persists for more than 15 seconds, the pipeline will stop with an error.
                warning!(CAT, obj: pad, "Dropping buffer because input PTS {} cannot be converted to the range {:?} to {:?}.",
                    input_pts.unwrap_or_default(), PravegaTimestamp::MIN, PravegaTimestamp::MAX);
                if input_pts > Some(ClockTime::from_seconds(15)) {
                    error!(CAT, obj: pad,
                        "Input buffers do not have valid PTS timestamps. \
                        If you are using an RTSP source, this may occur if the RTSP source is not sending RTCP Sender Reports. \
                        This can be worked around by setting the property {}={}. \
                        If launched with rtsp-camera-to-pravega.py, then set the environment variable TIMESTAMP_SOURCE=local-clock. \
                        Beware that this will reduce timestamp accuracy.",
                        PROPERTY_NAME_INPUT_TIMESTAMP_MODE, NICK_START_AT_CURRENT_TIME);
                    Err(gst::FlowError::Error)
                    }
                else {
                    Ok(gst::FlowSuccess::Ok)
                }
            };
            success
        } else {
            warning!(CAT, obj: pad, "Dropping buffer because PTS is none");
            Ok(gst::FlowSuccess::Ok)
        }
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        debug!(CAT, obj: pad, "sink_event: event={:?}", event);
        match event.view() {
            gst::EventView::Segment(segment) => {
                // Segments from a file will have a start and end timestamp which will prevent
                // playback after adjusting the PTS.
                // To avoid this, we replace the segment with an empty one.
                debug!(CAT, obj: pad, "sink_event: segment={:?}", segment);
                let new_segment = gst::FormattedSegment::<gst::ClockTime>::new();
                let new_event = gst::event::Segment::new(new_segment.as_ref());
                debug!(CAT, obj: pad, "sink_event: new_segment={:?}", new_segment);
                self.srcpad.push_event(new_event)
            }
            _ => self.srcpad.push_event(event)
        }        
    }

    fn sink_query(&self, _pad: &gst::Pad,  query: &mut gst::QueryRef) -> bool {
        self.srcpad.peer_query(query)
    }

    fn src_event(&self, _pad: &gst::Pad, event: gst::Event) -> bool {
        self.sinkpad.push_event(event)
    }

    fn src_query(&self, _pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        self.sinkpad.peer_query(query)
    }
}

#[glib::object_subclass]
impl ObjectSubclass for TimestampCvt {
    const NAME: &'static str = ELEMENT_CLASS_NAME;
    type Type = super::TimestampCvt;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_from_template(&templ)
            .chain_function(|pad, parent, buffer| {
                TimestampCvt::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |identity| identity.sink_chain(pad, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                TimestampCvt::catch_panic_pad_function(
                    parent,
                    || false,
                    |identity| identity.sink_event(pad, event),
                )
            })
            .query_function(|pad, parent, query| {
                TimestampCvt::catch_panic_pad_function(
                    parent,
                    || false,
                    |identity| identity.sink_query(pad, query),
                )
            })
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_from_template(&templ)
        .event_function(|pad, parent, event| {
            TimestampCvt::catch_panic_pad_function(
                parent,
                || false,
                |identity| identity.src_event(pad, event),
            )
        })
        .query_function(|pad, parent, query| {
            TimestampCvt::catch_panic_pad_function(
                parent,
                || false,
                |identity| identity.src_query(pad, query),
            )
        })
        .build();

        Self {
            settings: Mutex::new(Default::default()),
            state: Mutex::new(Default::default()),
            srcpad,
            sinkpad,
        }
    }
}

impl ObjectImpl for TimestampCvt {
    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }

    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| { vec![
            glib::ParamSpecEnum::builder_with_default(PROPERTY_NAME_INPUT_TIMESTAMP_MODE, DEFAULT_INPUT_TIMESTAMP_MODE)
                .nick("Input timestamp mode")
                .blurb("Timestamp mode used by the input")
                .mutable_ready()
                .build(),
            glib::ParamSpecString::builder(PROPERTY_NAME_START_UTC)
                .nick("Start UTC")
                .blurb("If input-timestamp-mode=start-at-fixed-time, this is the timestamp at which to start, \
                in RFC 3339 format. For example: 2021-12-28T23:41:45.691Z")
                .mutable_ready()
                .build(),
        ]});
        PROPERTIES.as_ref()
    }

    fn set_property(
        &self,
        _id: usize,
        value: &glib::Value,
        pspec: &glib::ParamSpec,
    ) {
        let obj = self.obj();
        match pspec.name() {
            PROPERTY_NAME_INPUT_TIMESTAMP_MODE => {
                let res: Result<(), glib::Error> = match value.get::<InputTimestampMode>() {
                    Ok(timestamp_mode) => {
                        let mut settings = self.settings.lock().unwrap();
                        settings.input_timestamp_mode = timestamp_mode;
                        Ok(())
                    },
                    Err(_) => unreachable!("type checked upstream"),
                };
                if let Err(err) = res {
                    error!(CAT, obj: obj, "Failed to set property `{}`: {}", PROPERTY_NAME_INPUT_TIMESTAMP_MODE, err);
                }
            },
            PROPERTY_NAME_START_UTC => {
                let res = match value.get::<String>() {
                    Ok(start_utc) => {
                        let mut settings = self.settings.lock().unwrap();
                        let timestamp = PravegaTimestamp::try_from(start_utc);
                        timestamp.map(|t| settings.start_timestamp = t.nanoseconds().unwrap())
                    },
                    Err(_) => unreachable!("type checked upstream"),
                };
                if let Err(err) = res {
                    error!(CAT, obj: obj, "Failed to set property `{}`: {}", PROPERTY_NAME_START_UTC, err);
                }
            },
        _ => unimplemented!(),
        };
    }
}

impl GstObjectImpl for TimestampCvt {}

impl ElementImpl for TimestampCvt {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                ELEMENT_LONG_NAME,
                "Generic",
                ELEMENT_DESCRIPTION,
                ELEMENT_AUTHOR,
                )
        });
        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::new_any();
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();
            vec![src_pad_template, sink_pad_template]
        });
        PAD_TEMPLATES.as_ref()
    }
}
