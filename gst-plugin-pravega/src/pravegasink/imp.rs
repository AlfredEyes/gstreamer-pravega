//
// Copyright (c) Dell Inc., or its subsidiaries. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//

// A sink that writes GStreamer buffers to a Pravega stream.
// Based on:
//   - https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/-/tree/master/generic/file/src/filesink

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{debug, error, fixme, info, log, trace, memdump};
use gst_base::subclass::prelude::*;

use std::cmp;
use std::convert::TryInto;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::sync::Mutex;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use std::sync::mpsc::{self, Sender, Receiver, RecvTimeoutError};

use once_cell::sync::Lazy;

use pravega_client::client_factory::ClientFactoryAsync;
use pravega_client::byte::ByteWriter;
use pravega_client_shared::{Scope, Stream, StreamConfiguration, ScopedStream, Scaling, ScaleType};
use pravega_video::event_serde::{EventWithHeader, EventWriter};
use pravega_video::index::{IndexRecord, IndexRecordWriter, IndexSearcher, SearchMethod, get_index_stream_name};
use pravega_video::timestamp::{PravegaTimestamp, SECOND};
use pravega_video::utils;
use pravega_video::utils::SyncByteReader;

use tokio::runtime::Runtime;

use crate::counting_writer::CountingWriter;
use crate::numeric::u64_to_i64_saturating_sub;
use crate::seekable_byte_stream_writer::SeekableByteWriter;

const PROPERTY_NAME_STREAM: &str = "stream";
const PROPERTY_NAME_CONTROLLER: &str = "controller";
const PROPERTY_NAME_SEAL: &str = "seal";
const PROPERTY_NAME_BUFFER_SIZE: &str = "buffer-size";
const PROPERTY_NAME_TIMESTAMP_MODE: &str = "timestamp-mode";
const PROPERTY_NAME_INDEX_MIN_SEC: &str = "index-min-sec";
const PROPERTY_NAME_INDEX_MAX_SEC: &str = "index-max-sec";
const PROPERTY_NAME_ALLOW_CREATE_SCOPE: &str = "allow-create-scope";
const PROPERTY_NAME_KEYCLOAK_FILE: &str = "keycloak-file";
const PROPERTY_NAME_RETENTION_TYPE: &str = "retention-type";
const PROPERTY_NAME_RETENTION_DAYS: &str = "retention-days";
const PROPERTY_NAME_RETENTION_BYTES: &str = "retention-bytes";
const PROPERTY_NAME_RETENTION_MAINTENANCE_INTERVAL_SECONDS: &str = "retention-maintenance-interval-seconds";

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstTimestampMode")]
pub enum TimestampMode {
    #[enum_value(
        name = "(DEPRECATED) Pipeline uses the realtime clock which provides nanoseconds \
                since the Unix epoch 1970-01-01 00:00:00 UTC, not including leap seconds. \
                This mode is deprecated. Instead, use the timestampcvt element with input-timestamp-mode=start-at-current-time.",
        nick = "realtime-clock"
    )]
    RealtimeClock = 0,
    #[enum_value(
        name = "(DEPRECATED) Input buffer timestamps are nanoseconds \
                since the NTP epoch 1900-01-01 00:00:00 UTC, not including leap seconds. \
                Use this for buffers from rtspsrc (ntp-sync=true ntp-time-source=running-time). \
                This mode is deprecated. Instead, use the timestampcvt element with input-timestamp-mode=ntp.",
        nick = "ntp"
    )]
    Ntp = 1,
    #[enum_value(
        name = "Input buffer timestamps are nanoseconds \
                since 1970-01-01 00:00:00 TAI International Atomic Time, including leap seconds.",
        nick = "tai"
    )]
    Tai = 2,
}

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstRetentionType")]
pub enum RetentionType {
    #[enum_value(
        name = "If 'none', no data will be deleted from the stream. ",
        nick = "none"
    )]
    None = 0,
    #[enum_value(
        name = "If 'days', data older than 'retention-days' will be deleted from the stream.",
        nick = "days"
    )]
    Days = 1,
    #[enum_value(
        name = "If 'bytes', the oldest data will be deleted so that the data size does not exceed 'retention-bytes'.",
        nick = "bytes"
    )]
    Bytes = 2,
    #[enum_value(
        name = "If 'daysAndBytes', the oldest data will be deleted if it is older than 'retention-days' or the data size exceeds 'retention-bytes'.",
        nick = "daysAndBytes"
    )]
    DaysAndBytes = 3,
}

#[derive(Debug)]
enum RetentionPolicy {
    Days(f64),
    Bytes(u64),
    DaysAndBytes(f64, u64),
    None,
}

impl RetentionPolicy {
    fn new(retention_type: RetentionType, days: Option<f64>, bytes: Option<u64>) -> Result<Self, String> {
        match retention_type {
            RetentionType::Days => days.ok_or(String::from("retention-days is not set")).map(|days| {Self::Days(days)}),
            RetentionType::Bytes => bytes.ok_or(String::from("retention-bytes is not set")).map(|bytes| {Self::Bytes(bytes)}),
            RetentionType::DaysAndBytes => {
                let days= days.ok_or(String::from("retention-days is not set"))?;
                let bytes = bytes.ok_or(String::from("retention-bytes is not set"))?;
                Ok(Self::DaysAndBytes(days, bytes))
            },
            RetentionType::None => Ok(Self::None),
        }
    }
}

struct RetentionMaintainer {
    element: super::PravegaSink,
    interval_seconds: u64,
    retention_policy: RetentionPolicy,
    factory: ClientFactoryAsync,
    index_searcher: IndexSearcher<SyncByteReader>,
    index_writer: ByteWriter,
    data_writer: ByteWriter,
}

impl RetentionMaintainer {
    fn new(element: super::PravegaSink, interval_seconds: u64, retention_policy: RetentionPolicy, factory: ClientFactoryAsync, index_scoped_stream: ScopedStream, data_scoped_stream: ScopedStream) -> Self {
        let index_reader = factory.runtime_handle().block_on(factory.create_byte_reader(index_scoped_stream.clone()));
        let index_writer = factory.runtime_handle().block_on(factory.create_byte_writer(index_scoped_stream));
        let data_writer = factory.runtime_handle().block_on(factory.create_byte_writer(data_scoped_stream));
        let index_searcher = IndexSearcher::new(SyncByteReader::new(index_reader, factory.runtime_handle()));
        Self {
            element,
            interval_seconds,
            retention_policy,
            factory,
            index_searcher,
            index_writer,
            data_writer,
        }
    }

    fn days_to_seconds(days: f64) -> i128 {
        let seconds = days * 24.0 * 60.0 * 60.0;
        seconds.round() as i128
    }

    fn run(mut self, thread_stop_rx: Receiver<()>) -> Option<JoinHandle<()>> {
        let (seconds, bytes) = match self.retention_policy {
            RetentionPolicy::Days(days) => (Some(RetentionMaintainer::days_to_seconds(days)), None),
            RetentionPolicy::Bytes(bytes) => (None, Some(bytes)),
            RetentionPolicy::DaysAndBytes(days, bytes) => (Some(RetentionMaintainer::days_to_seconds(days)), Some(bytes)),
            _ => (None, None),
        };

        if seconds.is_none() && bytes.is_none() {
            return None;
        }

        info!(CAT, obj: &self.element, "start: retention_maintainer_interval_seconds={}", self.interval_seconds);
        let handle = thread::spawn(move || {
            loop {
                if let Some(sec) = seconds {
                    let truncate_at_timestamp = PravegaTimestamp::now() - sec * SECOND;
                    info!(CAT, obj: &self.element, "Truncating prior to {}", truncate_at_timestamp);

                    let search_result = self.index_searcher.search_timestamp_and_return_index_offset(truncate_at_timestamp, SearchMethod::Before);
                    if let Ok(result) = search_result {
                        let runtime = self.factory.runtime_handle();
                        runtime.block_on(self.index_writer.truncate_data_before(result.1 as i64)).unwrap();
                        info!(CAT, obj: &self.element, "Index truncated at offset {}", result.1);
                        runtime.block_on(self.data_writer.truncate_data_before(result.0.offset as i64)).unwrap();
                        info!(CAT, obj: &self.element, "Data truncated at offset {}", result.0.offset);
                    }
                }

                if let Some(bytes) = bytes {
                    info!(CAT, obj: &self.element, "Truncating larger than {} bytes", bytes);

                    let search_result = self.index_searcher.search_size_and_return_index_offset(bytes, SearchMethod::Before);
                    if let Ok(result) = search_result {
                        let runtime = self.factory.runtime_handle();
                        runtime.block_on(self.index_writer.truncate_data_before(result.1 as i64)).unwrap();
                        info!(CAT, obj: &self.element, "Index truncated at offset {}", result.1);
                        runtime.block_on(self.data_writer.truncate_data_before(result.0.offset as i64)).unwrap();
                        info!(CAT, obj: &self.element, "Data truncated at offset {}", result.0.offset);
                    }
                }

                // break the loop to stop the thread
                match thread_stop_rx.recv_timeout(Duration::from_secs(self.interval_seconds)) {
                    Ok(_) | Err(RecvTimeoutError::Disconnected) => {
                        info!(CAT, obj: &self.element, "Retention maintainer thread terminated");
                        break;
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                }
            }
        });
        Some(handle)
    }
}

const DEFAULT_BUFFER_SIZE: usize = 128*1024;
const DEFAULT_TIMESTAMP_MODE: TimestampMode = TimestampMode::Tai;
const DEFAULT_INDEX_MIN_SEC: f64 = 0.5;
const DEFAULT_INDEX_MAX_SEC: f64 = 10.0;
const DEFAULT_RETENTION_TYPE: RetentionType = RetentionType::None;
const DEFAULT_RETENTION_MAINTENANCE_INTERVAL_SECONDS: u64 = 15 * 60;

#[derive(Debug)]
struct Settings {
    scope: Option<String>,
    stream: Option<String>,
    controller: Option<String>,
    seal: bool,
    buffer_size: usize,
    timestamp_mode: TimestampMode,
    index_min_nanos: u64,
    index_max_nanos: u64,
    allow_create_scope: bool,
    keycloak_file: Option<String>,
    retention_type: RetentionType,
    retention_days: Option<f64>,
    retention_bytes: Option<u64>,
    retention_maintenance_interval_seconds: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            scope: None,
            stream: None,
            controller: utils::default_pravega_controller_uri(),
            seal: false,
            buffer_size: DEFAULT_BUFFER_SIZE,
            timestamp_mode: DEFAULT_TIMESTAMP_MODE,
            index_min_nanos: (DEFAULT_INDEX_MIN_SEC * 1e9) as u64,
            index_max_nanos: (DEFAULT_INDEX_MAX_SEC * 1e9) as u64,
            allow_create_scope: true,
            keycloak_file: utils::default_keycloak_file(),
            retention_type: DEFAULT_RETENTION_TYPE,
            retention_days: None,
            retention_bytes: None,
            retention_maintenance_interval_seconds: DEFAULT_RETENTION_MAINTENANCE_INTERVAL_SECONDS,
        }
    }
}

enum State {
    Stopped,
    Started {
        runtime: Runtime,
        writer: CountingWriter<BufWriter<SeekableByteWriter>>,
        index_writer: SeekableByteWriter,
        // First received PTS that is not None.
        first_valid_time: PravegaTimestamp,
        // PTS of last written index record.
        last_index_time: PravegaTimestamp,
        // The timestamp that will be written to the index upon end-of-stream.
        final_timestamp: PravegaTimestamp,
        // The offset that will be written to the index upon end-of-stream.
        final_offset: Option<u64>,
        buffers_written: u64,
        retention_thread_stop_tx: Sender<()>,
        retention_thread_handle: Option<JoinHandle<()>>,
    },
}

impl Default for State {
    fn default() -> State {
        State::Stopped
    }
}

pub struct PravegaSink {
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "pravegasink",
        gst::DebugColorFlags::empty(),
        Some("Pravega Sink"),
    )
});

impl PravegaSink {
    fn set_stream(
        &self,
        _element: &super::PravegaSink,
        stream: Option<String>,
    ) -> Result<(), glib::Error> {
        let mut settings = self.settings.lock().unwrap();
        let (scope, stream) = match stream {
            Some(stream) => {
                let components: Vec<&str> = stream.split('/').collect();
                if components.len() != 2 {
                    return Err(glib::Error::new(
                        gst::URIError::BadUri,
                        format!("stream parameter '{}' is formatted incorrectly. It must be specified as scope/stream.", stream).as_str(),
                    ));
                }
                let scope = components[0].to_owned();
                let stream = components[1].to_owned();
                (Some(scope), Some(stream))
            }
            None => {
                info!(CAT, imp: self, "Resetting `{}` to None", PROPERTY_NAME_STREAM);
                (None, None)
            }
        };
        settings.scope = scope;
        settings.stream = stream;
        Ok(())
    }

    fn set_controller(
        &self,
        _element: &super::PravegaSink,
        controller: Option<String>,
    ) -> Result<(), glib::Error> {
        let mut settings = self.settings.lock().unwrap();
        settings.controller = controller;
        Ok(())
    }
}

#[glib::object_subclass]
impl ObjectSubclass for PravegaSink {
    const NAME: &'static str = "PravegaSink";
    type Type = super::PravegaSink;
    type ParentType = gst_base::BaseSink;

    fn new() -> Self {
        pravega_video::tracing::init();
        Self {
            settings: Mutex::new(Default::default()),
            state: Mutex::new(Default::default()),
        }
    }
}

impl GstObjectImpl for PravegaSink {}

impl ObjectImpl for PravegaSink {
    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.set_element_flags(gst::ElementFlags::PROVIDE_CLOCK | gst::ElementFlags::REQUIRE_CLOCK);
    }

    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| { vec![
            glib::ParamSpecString::builder(PROPERTY_NAME_STREAM)
                .nick("Stream")
                .blurb("scope/stream")
                .mutable_ready()
                .build(),
            glib::ParamSpecString::builder(PROPERTY_NAME_CONTROLLER)
                .nick("Controller")
                .blurb(format!("Pravega controller. \
                    If not specified, this will use the value of the environment variable {}. \
                    If that is empty, it will use the default of {}.",
                    utils::ENV_PRAVEGA_CONTROLLER_URI, utils::DEFAULT_PRAVEGA_CONTROLLER_URI).as_str())
                    .mutable_ready()
                .build(),
            glib::ParamSpecBoolean::builder(PROPERTY_NAME_SEAL)
                .nick("Seal")
                .blurb("Seal Pravega stream when stopped")
                .default_value(false)
                .mutable_ready()
                .build(),
            glib::ParamSpecUInt::builder(PROPERTY_NAME_BUFFER_SIZE)
                .nick("Buffer size")
                .blurb("Size of buffer in number of bytes")
                .minimum(0)
                .maximum(std::u32::MAX)
                .default_value(DEFAULT_BUFFER_SIZE.try_into().unwrap())
                .mutable_ready()
                .build(),
            glib::ParamSpecEnum::builder_with_default(PROPERTY_NAME_TIMESTAMP_MODE, DEFAULT_TIMESTAMP_MODE)
                .nick("Timestamp mode")
                .blurb("Timestamp mode used by the input")
                .mutable_ready()
                .build(),
            glib::ParamSpecDouble::builder(PROPERTY_NAME_INDEX_MIN_SEC)
                .nick("Minimum index interval")
                .blurb("The minimum number of seconds between index records")
                .minimum(0.0)
                .maximum(std::f64::INFINITY)
                .default_value(DEFAULT_INDEX_MIN_SEC.try_into().unwrap())
                .mutable_ready()
                .build(),
            glib::ParamSpecDouble::builder(PROPERTY_NAME_INDEX_MAX_SEC)
                .nick("Maximum index interval")
                .blurb("Force index record if one has not been created in this many seconds, even at delta frames.")
                .minimum(0.0)
                .maximum(std::f64::INFINITY)
                .default_value(DEFAULT_INDEX_MAX_SEC.try_into().unwrap())
                .mutable_ready()
                .build(),
            glib::ParamSpecBoolean::builder(PROPERTY_NAME_ALLOW_CREATE_SCOPE)
                .nick("Allow create scope")
                .blurb("If true, the Pravega scope will be created if needed.")
                .default_value(true)
                .mutable_ready()
                .build(),
            glib::ParamSpecString::builder(PROPERTY_NAME_KEYCLOAK_FILE)
                .nick("Keycloak file")
                .blurb(format!("The filename containing the Keycloak credentials JSON. \
                    If not specified, this will use the value of the environment variable {}. \
                    If that is empty, authentication will be disabled.",
                    utils::ENV_KEYCLOAK_SERVICE_ACCOUNT_FILE).as_str())
                .mutable_ready()
                .build(),
            glib::ParamSpecEnum::builder_with_default(PROPERTY_NAME_RETENTION_TYPE, DEFAULT_RETENTION_TYPE)
                .nick("Retention type")
                .blurb("If 'days', data older than 'retention-days' will be deleted from the stream. If 'bytes', the oldest data will be deleted so that the data size does not exceed 'retention-bytes'. If daysAndBytes, the oldest data will be deleted if it is older than retention-days or the data size exceeds retention-bytes.")
                .mutable_ready()
                .build(),
            glib::ParamSpecDouble::builder(PROPERTY_NAME_RETENTION_DAYS)
                .nick("Retention days")
                .blurb("The number of days that the video stream will be retained.")
                .minimum(0.0)
                .maximum(std::f64::INFINITY)
                .default_value(0.0)
                .mutable_ready()
                .build(),
            glib::ParamSpecUInt64::builder(PROPERTY_NAME_RETENTION_BYTES)
                .nick("Retention bytes")
                .blurb("The number of bytes that the video stream will be retained.")
                .minimum(0)
                .maximum(std::u64::MAX)
                .default_value(0)
                .mutable_ready()
                .build(),
            glib::ParamSpecUInt64::builder(PROPERTY_NAME_RETENTION_MAINTENANCE_INTERVAL_SECONDS)
                .nick("Retention maintenance interval seconds")
                .blurb("The oldest data will be deleted from the stream with this interval, according to the retention policy.")
                .minimum(0)
                .maximum(std::u64::MAX)
                .default_value(DEFAULT_RETENTION_MAINTENANCE_INTERVAL_SECONDS)
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
            PROPERTY_NAME_STREAM => {
                let res = match value.get::<String>() {
                    Ok(stream) => self.set_stream(&obj, Some(stream)),
                    Err(_) => unreachable!("type checked upstream"),
                };
                if let Err(err) = res {
                    error!(CAT, obj: obj, "Failed to set property `{}`: {}", PROPERTY_NAME_STREAM, err);
                }
            },
            PROPERTY_NAME_CONTROLLER => {
                let res = match value.get::<String>() {
                    Ok(controller) => {
                        let controller = if controller.is_empty() {
                            None
                        } else {
                            Some(controller)
                        };
                        self.set_controller(&obj, controller)
                    },
                    Err(_) => unreachable!("type checked upstream"),
                };
                if let Err(err) = res {
                    error!(CAT, obj: obj, "Failed to set property `{}`: {}", PROPERTY_NAME_CONTROLLER, err);
                }
            },
            PROPERTY_NAME_SEAL => {
                let res: Result<(), glib::Error> = match value.get::<bool>() {
                    Ok(seal) => {
                        let mut settings = self.settings.lock().unwrap();
                        settings.seal = seal;
                        Ok(())
                    },
                    Err(_) => unreachable!("type checked upstream"),
                };
                if let Err(err) = res {
                    error!(CAT, obj: obj, "Failed to set property `{}`: {}", PROPERTY_NAME_SEAL, err);
                }
            },
            PROPERTY_NAME_BUFFER_SIZE => {
                let res: Result<(), glib::Error> = match value.get::<u32>() {
                    Ok(buffer_size) => {
                        let mut settings = self.settings.lock().unwrap();
                        settings.buffer_size = buffer_size.try_into().unwrap_or_default();
                        Ok(())
                    },
                    Err(_) => unreachable!("type checked upstream"),
                };
                if let Err(err) = res {
                    error!(CAT, obj: obj, "Failed to set property `{}`: {}", PROPERTY_NAME_BUFFER_SIZE, err);
                }
            },
            PROPERTY_NAME_TIMESTAMP_MODE => {
                let res: Result<(), glib::Error> = match value.get::<TimestampMode>() {
                    Ok(timestamp_mode) => {
                        let mut settings = self.settings.lock().unwrap();
                        settings.timestamp_mode = timestamp_mode;
                        Ok(())
                    },
                    Err(_) => unreachable!("type checked upstream"),
                };
                if let Err(err) = res {
                    error!(CAT, obj: obj, "Failed to set property `{}`: {}", PROPERTY_NAME_TIMESTAMP_MODE, err);
                }
            },
            PROPERTY_NAME_INDEX_MIN_SEC => {
                let res: Result<(), glib::Error> = match value.get::<f64>() {
                    Ok(index_min_sec) => {
                        let mut settings = self.settings.lock().unwrap();
                        settings.index_min_nanos = (index_min_sec * 1e9) as u64;
                        Ok(())
                    },
                    Err(_) => unreachable!("type checked upstream"),
                };
                if let Err(err) = res {
                    error!(CAT, obj: obj, "Failed to set property `{}`: {}", PROPERTY_NAME_INDEX_MIN_SEC, err);
                }
            },
            PROPERTY_NAME_INDEX_MAX_SEC => {
                let res: Result<(), glib::Error> = match value.get::<f64>() {
                    Ok(index_max_sec) => {
                        let mut settings = self.settings.lock().unwrap();
                        settings.index_max_nanos = (index_max_sec * 1e9) as u64;
                        Ok(())
                    },
                    Err(_) => unreachable!("type checked upstream"),
                };
                if let Err(err) = res {
                    error!(CAT, obj: obj, "Failed to set property `{}`: {}", PROPERTY_NAME_INDEX_MAX_SEC, err);
                }
            },
            PROPERTY_NAME_ALLOW_CREATE_SCOPE => {
                let res: Result<(), glib::Error> = match value.get::<bool>() {
                    Ok(allow_create_scope) => {
                        let mut settings = self.settings.lock().unwrap();
                        settings.allow_create_scope = allow_create_scope;
                        Ok(())
                    },
                    Err(_) => unreachable!("type checked upstream"),
                };
                if let Err(err) = res {
                    error!(CAT, obj: obj, "Failed to set property `{}`: {}", PROPERTY_NAME_ALLOW_CREATE_SCOPE, err);
                }
            },
            PROPERTY_NAME_KEYCLOAK_FILE => {
                let res: Result<(), glib::Error> = match value.get::<String>() {
                    Ok(keycloak_file) => {
                        let mut settings = self.settings.lock().unwrap();
                        settings.keycloak_file = if keycloak_file.is_empty() {
                            None
                        } else {
                            Some(keycloak_file)
                        };
                        Ok(())
                    },
                    Err(_) => unreachable!("type checked upstream"),
                };
                if let Err(err) = res {
                    error!(CAT, obj: obj, "Failed to set property `{}`: {}", PROPERTY_NAME_KEYCLOAK_FILE, err);
                }
            },
            PROPERTY_NAME_RETENTION_TYPE => {
                let res: Result<(), glib::Error> = match value.get::<RetentionType>() {
                    Ok(retention_type) => {
                        let mut settings = self.settings.lock().unwrap();
                        settings.retention_type = retention_type;
                        Ok(())
                    },
                    Err(_) => unreachable!("type checked upstream"),
                };
                if let Err(err) = res {
                    error!(CAT, obj: obj, "Failed to set property `{}`: {}", PROPERTY_NAME_RETENTION_TYPE, err);
                }
            },
            PROPERTY_NAME_RETENTION_DAYS => {
                let res: Result<(), glib::Error> = match value.get::<f64>() {
                    Ok(days) => {
                        let mut settings = self.settings.lock().unwrap();
                        settings.retention_days = Some(days);
                        Ok(())
                    },
                    Err(_) => unreachable!("type checked upstream"),
                };
                if let Err(err) = res {
                    error!(CAT, obj: obj, "Failed to set property `{}`: {}", PROPERTY_NAME_RETENTION_DAYS, err);
                }
            },
            PROPERTY_NAME_RETENTION_BYTES => {
                let res: Result<(), glib::Error> = match value.get::<u64>() {
                    Ok(bytes) => {
                        let mut settings = self.settings.lock().unwrap();
                        settings.retention_bytes = Some(bytes);
                        Ok(())
                    },
                    Err(_) => unreachable!("type checked upstream"),
                };
                if let Err(err) = res {
                    error!(CAT, obj: obj, "Failed to set property `{}`: {}", PROPERTY_NAME_RETENTION_BYTES, err);
                }
            },
            PROPERTY_NAME_RETENTION_MAINTENANCE_INTERVAL_SECONDS => {
                let res: Result<(), glib::Error> = match value.get::<u64>() {
                    Ok(seconds) => {
                        let mut settings = self.settings.lock().unwrap();
                        settings.retention_maintenance_interval_seconds = seconds;
                        Ok(())
                    },
                    Err(_) => unreachable!("type checked upstream"),
                };
                if let Err(err) = res {
                    error!(CAT, obj: obj, "Failed to set property `{}`: {}", PROPERTY_NAME_RETENTION_MAINTENANCE_INTERVAL_SECONDS, err);
                }
            },       
        _ => unimplemented!(),
        };
    }
}

impl ElementImpl for PravegaSink {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Pravega Sink",
                "Sink/Pravega",
                "Write to a Pravega stream",
                "Claudio Fahey <claudio.fahey@dell.com>",
            )
        });
        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::new_any();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![sink_pad_template]
        });
        PAD_TEMPLATES.as_ref()
    }

    // We always want to use the realtime (Unix) clock, although it is ignored when timestamp-mode=ntp or tai.
    fn provide_clock(&self) -> Option<gst::Clock> {
        let clock = gst::SystemClock::obtain();
        let clock_type = gst::ClockType::Realtime;
        clock.set_property("clock-type", &clock_type);
        let time = clock.time();
        info!(CAT, imp: self, "provide_clock: Using clock_type={:?}, time={}, ({} ns)", clock_type, time.unwrap(), time.unwrap().nseconds());
        Some(clock)
    }
}

impl BaseSinkImpl for PravegaSink {
    fn start(&self) -> Result<(), gst::ErrorMessage> {
        debug!(CAT, imp: self, "start: BEGIN");
        let result = (|| {
            let mut state = self.state.lock().unwrap();
            if let State::Started { .. } = *state {
                unreachable!("PravegaSink already started");
            }

            let settings = self.settings.lock().unwrap();
            info!(CAT, imp: self, "start: index_min_nanos={}, index_max_nanos={}", settings.index_min_nanos, settings.index_max_nanos);
            if !(settings.index_min_nanos <= settings.index_max_nanos) {
                return Err(gst::error_msg!(gst::ResourceError::Settings,
                    ["{} must be <= {}", PROPERTY_NAME_INDEX_MIN_SEC, PROPERTY_NAME_INDEX_MAX_SEC]))
            };
            let scope_name: String = settings.scope.clone().ok_or_else(|| {
                gst::error_msg!(gst::ResourceError::Settings, ["Scope is not defined"])
            })?;
            let stream_name = settings.stream.clone().ok_or_else(|| {
                gst::error_msg!(gst::ResourceError::Settings, ["Stream is not defined"])
            })?;
            let index_stream_name = get_index_stream_name(&stream_name);
            let scope = Scope::from(scope_name);
            let stream = Stream::from(stream_name);
            let index_stream = Stream::from(index_stream_name);
            info!(CAT, imp: self, "start: scope={}, stream={}, index_stream={}", scope, stream, index_stream);
            info!(CAT, imp: self, "start: timestamp_mode={:?}", settings.timestamp_mode);

            let controller = settings.controller.clone().ok_or_else(|| {
                gst::error_msg!(gst::ResourceError::Settings, ["Controller is not defined"])
            })?;
            info!(CAT, imp: self, "start: controller={}", controller);
            let keycloak_file = settings.keycloak_file.clone();
            info!(CAT, imp: self, "start: keycloak_file={:?}", keycloak_file);
            let config = utils::create_client_config(controller, keycloak_file).map_err(|error| {
                gst::error_msg!(gst::ResourceError::Settings, ["Failed to create Pravega client config: {}", error])
            })?;
            trace!(CAT, imp: self, "start: config={:?}", config);
            info!(CAT, imp: self, "start: controller_uri={}:{}", config.controller_uri.domain_name(), config.controller_uri.port());
            info!(CAT, imp: self, "start: is_tls_enabled={}", config.is_tls_enabled);
            info!(CAT, imp: self, "start: is_auth_enabled={}", config.is_auth_enabled);

            let runtime = Runtime::new().unwrap();
            let client_factory = ClientFactoryAsync::new(config, runtime.handle().to_owned());
            let controller_client = client_factory.controller_client();

            // Create scope.
            info!(CAT, imp: self, "start: allow_create_scope={}", settings.allow_create_scope);
            if settings.allow_create_scope {
                // This is expected to fail in some environments, even if the scope already exists.
                // We will log the error and continue.
                let _ = runtime.block_on(controller_client.create_scope(&scope)).map_err(|error| {
                    debug!(CAT, imp: self, "Failed to create Pravega scope. This is normal if the scope already exists: {:?}", error);
                });
            }

            // Create data stream.
            let stream_config = StreamConfiguration {
                scoped_stream: ScopedStream {
                    scope: scope.clone(),
                    stream: stream.clone(),
                },
                scaling: Scaling {
                    scale_type: ScaleType::FixedNumSegments,
                    min_num_segments: 1,
                    ..Default::default()
                },
                retention: Default::default(),
                tags: utils::get_video_tags(),
            };
            runtime.block_on(controller_client.create_stream(&stream_config)).map_err(|error| {
                gst::error_msg!(gst::ResourceError::Settings, ["Failed to create Pravega data stream: {:?}", error])
            })?;

            // Create index stream.
            let index_stream_config = StreamConfiguration {
                scoped_stream: ScopedStream {
                    scope: scope.clone(),
                    stream: index_stream.clone(),
                },
                scaling: Scaling {
                    scale_type: ScaleType::FixedNumSegments,
                    min_num_segments: 1,
                    ..Default::default()
                },
                retention: Default::default(),
                tags: None,
            };
            runtime.block_on(controller_client.create_stream(&index_stream_config)).map_err(|error| {
                gst::error_msg!(gst::ResourceError::Settings, ["Failed to create Pravega index stream: {:?}", error])
            })?;

            let scoped_stream = ScopedStream {
                scope: scope.clone(),
                stream: stream.clone(),
            };
            let writer = runtime.block_on(client_factory.create_byte_writer(scoped_stream.clone()));
            let mut seekable_writer = SeekableByteWriter::new(writer, runtime.handle().to_owned());
            info!(CAT, imp: self, "start: Opened Pravega writer for data");
            seekable_writer.seek_to_tail();

            let index_scoped_stream = ScopedStream {
                scope: scope.clone(),
                stream: index_stream.clone(),
            };
            let index_writer = runtime.block_on(client_factory.create_byte_writer(index_scoped_stream.clone()));
            let mut index_writer = SeekableByteWriter::new(index_writer, runtime.handle().to_owned());
            info!(CAT, imp: self, "start: Opened Pravega writer for index");
            index_writer.seek_to_tail();

            info!(CAT, imp: self, "start: Buffer size is {}", settings.buffer_size);
            let buf_writer = BufWriter::with_capacity(settings.buffer_size, seekable_writer);
            let counting_writer = CountingWriter::new(buf_writer).unwrap();

            let retention_policy = RetentionPolicy::new(settings.retention_type, settings.retention_days, settings.retention_bytes).map_err(|error| {
                gst::error_msg!(gst::ResourceError::Settings, ["Failed to create retention policy: {}", error])
            })?;
            info!(CAT, imp: self, "start: retention_policy={:?}", retention_policy);

            let retention_maintainer = RetentionMaintainer::new(self.obj().clone(), settings.retention_maintenance_interval_seconds, retention_policy, client_factory.clone(),
                index_scoped_stream, scoped_stream);
            let (retention_thread_stop_tx, retention_thread_stop_rx) = mpsc::channel();
            let retention_thread_handle = retention_maintainer.run(retention_thread_stop_rx);

            *state = State::Started {
                runtime,
                writer: counting_writer,
                index_writer,
                first_valid_time: PravegaTimestamp::NONE,
                last_index_time: PravegaTimestamp::NONE,
                final_timestamp: PravegaTimestamp::NONE,
                final_offset: None,
                buffers_written: 0,
                retention_thread_stop_tx,
                retention_thread_handle,
            };
            info!(CAT, imp: self, "start: Started");
            Ok(())
        })();
        debug!(CAT, imp: self, "start: END; result={:?}", result);
        result
    }

    fn render(
        &self,
        buffer: &gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        trace!(CAT, imp: self, "render: BEGIN: Rendering {:?}", buffer);
        let result = (|| {
            let mut state = self.state.lock().unwrap();
            let (writer,
                index_writer,
                first_valid_time,
                last_index_time,
                final_timestamp,
                final_offset,
                buffers_written) = match *state {
                State::Started {
                    ref mut writer,
                    ref mut index_writer,
                    ref mut first_valid_time,
                    ref mut last_index_time,
                    ref mut final_timestamp,
                    ref mut final_offset,
                    ref mut buffers_written,
                    ..
                } => (writer,
                    index_writer,
                    first_valid_time,
                    last_index_time,
                    final_timestamp,
                    final_offset,
                    buffers_written),
                State::Stopped => {
                    gst::element_imp_error!(self, gst::CoreError::Failed, ["Not started yet"]);
                    return Err(gst::FlowError::Error);
                }
            };

            let pts = buffer.pts();
            let duration = buffer.duration();

            let map = buffer.map_readable().map_err(|_| {
                gst::element_imp_error!(self, gst::CoreError::Failed, ["Failed to map buffer"]);
                gst::FlowError::Error
            })?;
            let payload = map.as_ref();

            let (timestamp_mode, index_min_nanos, index_max_nanos) = {
                let settings = self.settings.lock().unwrap();
                (settings.timestamp_mode, settings.index_min_nanos, settings.index_max_nanos)
            };

            let timestamp = match timestamp_mode {
                TimestampMode::RealtimeClock => {
                    // pts is time between beginning of play and beginning of this buffer.
                    // base_time is the value of the pipeline clock (time since Unix epoch) at the beginning of play.
                    let unix_ts = match (self.obj().base_time(), pts) {
                        (Some(x), Some(y)) => Some(x + y),
                        _ => None,
                    };
                    PravegaTimestamp::from_unix_nanoseconds(unix_ts.map(gst::ClockTime::nseconds))
                },
                TimestampMode::Ntp => {
                    // When receiving from rtspsrc (ntp-sync=true ntp-time-source=running-time),
                    // pts will be the number of nanoseconds since the NTP epoch 1900-01-01 00:00:00 UTC
                    // of when the video frame was observed by the camera.
                    // Note: base_time is the value of the pipeline clock at the beginning of play. It is ignored.
                    PravegaTimestamp::from_ntp_nanoseconds(pts.map(gst::ClockTime::nseconds))
                },
                TimestampMode::Tai => {
                    PravegaTimestamp::from_nanoseconds(pts.map(gst::ClockTime::nseconds))
                }
            };

            if first_valid_time.is_none() {
                *first_valid_time = timestamp;
            }

            // Get the writer offset before writing. This offset will be used in the index.
            let writer_offset = writer.seek(SeekFrom::Current(0)).unwrap();

            log!(CAT, imp: self, "render: timestamp={:?}, pts={}, base_time={}, duration={}, size={}, writer_offset={}",
                timestamp, pts.unwrap_or_default(), self.obj().base_time().unwrap_or_default(), buffer.duration().unwrap_or_default(), buffer.size(), writer_offset);

            // We only want to include key frames (non-delta units) in the index.
            // However, if no key frame has been received in a while, force an index record.
            // This is required for nvv4l2h264enc because it identifies all buffers as DELTA_UNIT.
            let buffer_flags = buffer.flags();
            let is_delta_unit = buffer_flags.contains(gst::BufferFlags::DELTA_UNIT);
            let random_access = !is_delta_unit;
            let include_in_index = match timestamp.nanoseconds() {
                Some(timestamp) => {
                    match last_index_time.nanoseconds() {
                        Some(last_index_time) => {
                            let interval_sec = u64_to_i64_saturating_sub(timestamp, last_index_time) as f64 * 1e-9;
                            if is_delta_unit {
                                // We are at a delta frame.
                                if timestamp > last_index_time + index_max_nanos {
                                    fixme!(CAT, imp: self,
                                        "render: Forcing index record at delta unit because no key frame has been received for {} sec", interval_sec);
                                    true
                                } else {
                                    false
                                }
                            } else {
                                // We are at a key frame.
                                if timestamp < last_index_time + index_min_nanos {
                                    debug!(CAT, imp: self,
                                        "render: Skipping creation of index record because an index record was created {} sec ago", interval_sec);
                                    false
                                } else {
                                    debug!(CAT, imp: self,
                                        "render: Creating index record at key frame; last index record was created {} sec ago", interval_sec);
                                    true
                                }
                            }
                        },
                        None => {
                            // An index record has not been written by this element yet.
                            // The timestamp is valid.
                            if random_access {
                                true
                            } else {
                                // We are at a delta frame.
                                // Do not write an index record. unless no index record has been written for a while.
                                match first_valid_time.nanoseconds() {
                                    Some(first_valid_time) => {
                                        if timestamp > first_valid_time + index_max_nanos {
                                            let interval_sec = u64_to_i64_saturating_sub(timestamp, first_valid_time) as f64 * 1e-9;
                                            fixme!(CAT, imp: self,
                                                "render: Forcing first index record at delta unit because no key frame has been received for {} sec", interval_sec);
                                            true
                                        } else {
                                            false
                                        }
                                    },
                                    None => {
                                        // Should be unreachable.
                                        false
                                    },
                                }
                            }
                        },
                    }
                },
                None => {
                    // Buffer has an invalid timestamp. Never index.
                    false
                },
            };

            // Per the index constraints defined in index.rs, if we are writing an index record now,
            // we must flush any data writes prior to this buffer, so that reads do not block waiting on this writer.
            let flush = include_in_index;
            if flush {
                writer.flush().map_err(|error| {
                    gst::element_imp_error!(self, gst::CoreError::Failed, ["Failed to flush Pravega data stream: {}", error]);
                    gst::FlowError::Error
                })?;
            }

            // In order to detect any stalls writing the index stream, flush the index stream.
            // This will wait for all previous index records to be durably persisted.
            if flush {
                index_writer.flush().map_err(|error| {
                    gst::element_imp_error!(self, gst::CoreError::Failed, ["Failed to flush Pravega index stream: {}", error]);
                    gst::FlowError::Error
                })?;
            }

            // Record a discontinuity if any of the following are true:
            //   1) upstream has indicated a discontinuity (or resync) in the buffer
            //   3) this will be the first buffer written to the data stream from this instance
            //   2) this will be the first index record written from this instance
            let discontinuity =
                   buffer_flags.contains(gst::BufferFlags::DISCONT)
                || buffer_flags.contains(gst::BufferFlags::RESYNC)
                || *buffers_written == 0
                || (include_in_index && last_index_time.nanoseconds().is_none());
            if discontinuity {
                debug!(CAT, imp: self, "render: Recording discontinuity");
            }

            // Write index record.
            // We write the index record before the buffer so that any readers blocked on reading the
            // index will unblock as soon as possible.
            if include_in_index {
                let index_record = IndexRecord::new(timestamp, writer_offset,
                    random_access, discontinuity);
                let mut index_record_writer = IndexRecordWriter::new();
                index_record_writer.write(&index_record, index_writer).map_err(|err| {
                    gst::element_imp_error!(
                        self,
                        gst::ResourceError::Write,
                        ["Failed to write index: {}", err]
                    );
                    gst::FlowError::Error
                })?;
                debug!(CAT, imp: self, "render: Wrote index record {:?}", index_record);
                *last_index_time = timestamp;
            }

            // Write buffer to Pravega byte stream.
            // If buffer is greater than ~8 MiB, it will be fragmented into multiple atomic writes, each with an EventHeader.
            // Once fragmented, buffers will not be reassembled by pravegasrc.
            // However, demuxers such as qtdemux can correctly handled fragmented buffers.
            // In the event of an ungraceful pravegasink termination before all fragments are written,
            // it will mark the first buffer after starting as a discontinuity,
            // allowing elements downstream from pravegasrc to reinitialize.
            let mut pos_to_write = 0;
            loop {
                let length_to_write = usize::min(payload.len() - pos_to_write, EventWithHeader::max_payload_size());
                if length_to_write == 0 { break };
                let event = if pos_to_write == 0 {
                    EventWithHeader::new(&payload[pos_to_write..pos_to_write+length_to_write],
                        timestamp, include_in_index, random_access, discontinuity)
                } else {
                    debug!(CAT, imp: self, "render: buffer exceeds atomic write size and has been fragmented; writing additional payload of {} bytes", length_to_write);
                    // Additional writes must not be indexed and must not be marked as a discontinuity as that would reset the demuxer.
                    EventWithHeader::new(&payload[pos_to_write..pos_to_write+length_to_write],
                        timestamp, false, false, false)
                };
                memdump!(CAT, imp: self, "render: writing event={:?}", event);
                let mut event_writer = EventWriter::new();
                event_writer.write(&event, writer).map_err(|err| {
                    gst::element_imp_error!(
                        self,
                        gst::ResourceError::Write,
                        ["Failed to write buffer: {}", err]
                    );
                    gst::FlowError::Error
                })?;
                pos_to_write += length_to_write;
            }
            *buffers_written += 1;

            // Get the writer offset after writing.
            let writer_offset_end = writer.seek(SeekFrom::Current(0)).unwrap();
            trace!(CAT, imp: self, "render: wrote {} bytes from offset {} to {}",
                writer_offset_end - writer_offset, writer_offset, writer_offset_end);

            // Flush after writing if the buffer contains the SYNC_AFTER flag. This is normally not used.
            let sync_after = buffer_flags.contains(gst::BufferFlags::SYNC_AFTER);
            if sync_after {
                writer.flush().map_err(|error| {
                    gst::element_imp_error!(self, gst::CoreError::Failed, ["Failed to flush Pravega data stream: {}", error]);
                    gst::FlowError::Error
                })?;
                index_writer.flush().map_err(|error| {
                    gst::element_imp_error!(self, gst::CoreError::Failed, ["Failed to flush Pravega index stream: {}", error]);
                    gst::FlowError::Error
                })?;
                debug!(CAT, imp: self, "render: Streams flushed because SYNC_AFTER flag was set");
            }

            // Maintain values that may be written to the index on end-of-stream.
            // Per the index constraints defined in index.rs, the timestamp in the index record must
            // be strictly greater than the timestamp in the data stream.
            if timestamp.is_some() {
                // If duration of the buffer is reported as 0, we record it as if it had a 1 nanosecond duration.
                let duration = cmp::max(1, duration.unwrap_or_default().nseconds());
                *final_timestamp = PravegaTimestamp::from_nanoseconds(
                    timestamp.nanoseconds().map(|t| t + duration));
            }
            *final_offset = Some(writer_offset_end);

            Ok(gst::FlowSuccess::Ok)
        })();
        trace!(CAT, imp: self, "render: END: result={:?}", result);
        result
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        info!(CAT, imp: self, "stop: BEGIN");
        let result = (|| {
            let seal = {
                let settings = self.settings.lock().unwrap();
                settings.seal
            };

            let mut state = self.state.lock().unwrap();
            let (runtime,
                writer,
                index_writer,
                final_timestamp,
                final_offset,
                retention_thread_stop_tx,
                retention_thread_handle) = match *state {
                State::Started {
                    ref runtime,
                    ref mut writer,
                    ref mut index_writer,
                    ref mut final_timestamp,
                    ref mut final_offset,
                    ref mut retention_thread_stop_tx,
                    ref mut retention_thread_handle,
                    ..
                } => (runtime,
                    writer,
                    index_writer,
                    final_timestamp,
                    final_offset,
                    retention_thread_stop_tx,
                    retention_thread_handle),
                State::Stopped => {
                    return Err(gst::error_msg!(
                        gst::ResourceError::Settings,
                        ["PravegaSink not started"]
                    ));
                }
            };

            writer.flush().map_err(|error| {
                gst::error_msg!(gst::ResourceError::Write, ["Failed to flush Pravega data stream: {}", error])
            })?;

            // Write final index record.
            // The timestamp will be the the buffer timestamp + duration of the final buffer.
            // The offset will be current write position.
            if let Some(final_offset) = *final_offset {
                if final_timestamp.is_some() {
                    let index_record = IndexRecord::new(*final_timestamp, final_offset,
                        false, false);
                    let mut index_record_writer = IndexRecordWriter::new();
                    index_record_writer.write(&index_record, index_writer).map_err(|error| {
                        gst::error_msg!(gst::ResourceError::Write, ["Failed to write Pravega index stream: {}", error])
                    })?;
                    info!(CAT, imp: self, "stop: Wrote final index record {:?}", index_record);
                }
            }

            index_writer.flush().map_err(|error| {
                gst::error_msg!(gst::ResourceError::Write, ["Failed to flush Pravega index stream: {}", error])
            })?;

            if seal {
                info!(CAT, imp: self, "stop: Sealing streams");
                let writer = writer.get_mut().get_mut().get_mut();
                runtime.block_on(writer.seal()).map_err(|error| {
                    gst::error_msg!(gst::ResourceError::Write, ["Failed to seal Pravega data stream: {}", error])
                })?;
                index_writer.seal().map_err(|error| {
                    gst::error_msg!(gst::ResourceError::Write, ["Failed to seal Pravega index stream: {}", error])
                })?;
                info!(CAT, imp: self, "stop: Streams sealed");
            }

            // notify to stop the retention maintainer thread
            if let Some(_) = retention_thread_handle {
                let _ = retention_thread_stop_tx.send(());
                retention_thread_handle.take().map(JoinHandle::join);
            }

            *state = State::Stopped;
            Ok(())
        })();
        info!(CAT, imp: self, "stop: END: result={:?}", result);
        result
    }
}
