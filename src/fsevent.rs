//! Watcher implementation for Darwin's `FSEvents` API
//!
//! The `FSEvents` API provides a mechanism to notify clients about directories they ought to re-scan
//! in order to keep their internal data structures up-to-date with respect to the true state of
//! the file system. (For example, when files or directories are created, modified, or removed.) It
//! sends these notifications "in bulk", possibly notifying the client of changes to several
//! directories in a single callback.
//!
//! For more information see the [`FSEvents` API reference][ref].
//!
//! [ref]: https://developer.apple.com/library/mac/documentation/Darwin/Reference/FSEvents_Ref/
#![allow(clippy::borrow_interior_mutable_const, clippy::cast_possible_wrap)]

use std::ffi::c_void;
use std::io;
use std::panic::catch_unwind;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::mpsc::channel;
use std::task::{Context, Poll};
use std::thread;
use std::time::Duration;

use core_foundation::array::CFArray;
use core_foundation::base::{CFIndex, FromVoid};
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::runloop::{kCFRunLoopBeforeWaiting, kCFRunLoopDefaultMode, CFRunLoop};
use core_foundation::string::CFString;
use futures::stream::{abortable, AbortHandle, Abortable};
use futures::{Stream, StreamExt};
use log::{debug, error};
use tokio_stream::wrappers::ReceiverStream;

use crate::flags::StreamFlags;
use crate::impl_release_callback;
use crate::observer::create_oneshot_observer;
use crate::raw as fs;
use crate::raw::{
    kFSEventStreamCreateFlagUseCFTypes, kFSEventStreamCreateFlagUseExtendedData,
    kFSEventStreamEventExtendedDataPathKey, kFSEventStreamEventExtendedFileIDKey, CFRunLoopExt,
    FSEventStream, FSEventStreamContext, FSEventStreamCreateFlags, FSEventStreamEventFlags,
    FSEventStreamEventId,
};

/// An owned permission to stop a `RawEventStream` and terminate its backing `RunLoop`.
///
/// A `RawEventStreamHandler` *detaches* the associated Stream and `RunLoop` when it is dropped, which
/// means that there is no longer any handle to them and no way to `abort` them, which may cause
/// memory leaks.
pub struct RawEventStreamHandler {
    runloop: Option<(CFRunLoop, thread::JoinHandle<()>, AbortHandle)>,
}

impl RawEventStreamHandler {
    /// Stop a `RawEventStream` and terminate its backing `RunLoop`.
    pub fn abort(&mut self) {
        if let Some((runloop, thread_handle, abort_handle)) = self.runloop.take() {
            let (tx, rx) = channel();
            let observer = create_oneshot_observer(kCFRunLoopBeforeWaiting, tx);
            runloop.add_observer(&observer, unsafe { kCFRunLoopDefaultMode });

            if !runloop.is_waiting() {
                // Wait the RunLoop to enter Waiting state.
                rx.recv().expect("channel to receive BeforeWaiting signal");
            }

            runloop.remove_observer(&observer, unsafe { kCFRunLoopDefaultMode });
            runloop.stop();

            // Wait for the thread to shut down.
            thread_handle.join().expect("thread to shut down");

            // Abort the stream.
            abort_handle.abort();
        }
    }
}

#[derive(Debug, Clone)]
pub struct RawEvent {
    pub path: PathBuf,
    pub inode: i64,
    pub flags: StreamFlags,
    pub raw_flags: FSEventStreamEventFlags,
    pub id: FSEventStreamEventId,
}

pub struct RawEventStream {
    stream: Abortable<ReceiverStream<RawEvent>>,
}

impl Stream for RawEventStream {
    type Item = RawEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.stream.poll_next_unpin(cx)
    }
}

struct StreamContextInfo {
    event_handler: tokio::sync::mpsc::Sender<RawEvent>,
}

impl_release_callback!(release_context, StreamContextInfo);

struct SendWrapper<T>(T);

unsafe impl<T> Send for SendWrapper<T> {}

impl<T> SendWrapper<T> {
    const unsafe fn new(t: T) -> Self {
        Self(t)
    }
}

/// Create a new `RawEventStream` and `RawEventStreamHandler` pair.
///
/// # Errors
/// Return error when there's any invalid path in `paths_to_watch`.
pub fn raw_event_stream<P: AsRef<Path>>(
    paths_to_watch: impl IntoIterator<Item = P>,
    since_when: FSEventStreamEventId,
    latency: Duration,
    flags: FSEventStreamCreateFlags,
) -> io::Result<(RawEventStream, RawEventStreamHandler)> {
    let (event_tx, event_rx) = tokio::sync::mpsc::channel(1024);

    // We need to associate the stream context with our callback in order to propagate events
    // to the rest of the system. This will be owned by the stream, and will be freed when the
    // stream is closed. This means we will leak the context if we panic before reacing
    // `FSEventStreamRelease`.
    let context = StreamContextInfo {
        event_handler: event_tx,
    };

    let stream_context = FSEventStreamContext::new(context, release_context);

    // We must append some additional flags because our callback parse them so
    let mut stream = FSEventStream::new(
        callback,
        &stream_context,
        paths_to_watch,
        since_when,
        latency,
        flags | kFSEventStreamCreateFlagUseCFTypes | kFSEventStreamCreateFlagUseExtendedData,
    )?;

    // channel to pass runloop around
    let (runloop_tx, runloop_rx) = channel();

    let thread_handle = thread::spawn(move || {
        let current_runloop = CFRunLoop::get_current();

        stream.schedule(&current_runloop, unsafe { kCFRunLoopDefaultMode });
        stream.start();

        // the calling to CFRunLoopRun will be terminated by CFRunLoopStop call in drop()
        // SAFETY: `CF_REF` is thread-safe.
        runloop_tx
            .send(unsafe { SendWrapper::new(current_runloop) })
            .expect("send runloop to stream");

        CFRunLoop::run_current();
        stream.stop();
        stream.invalidate();
    });

    let (stream, stream_handle) = abortable(ReceiverStream::new(event_rx));
    Ok((
        RawEventStream { stream },
        RawEventStreamHandler {
            runloop: Some((
                runloop_rx.recv().expect("receive runloop from worker").0,
                thread_handle,
                stream_handle,
            )),
        },
    ))
}

extern "C" fn callback(
    stream_ref: fs::FSEventStreamRef,
    info: *mut c_void,
    num_events: usize,                               // size_t numEvents
    event_paths: *mut c_void,                        // void *eventPaths
    event_flags: *const fs::FSEventStreamEventFlags, // const FSEventStreamEventFlags eventFlags[]
    event_ids: *const fs::FSEventStreamEventId,      // const FSEventStreamEventId eventIds[]
) {
    drop(catch_unwind(move || {
        callback_impl(
            stream_ref,
            info,
            num_events,
            event_paths,
            event_flags,
            event_ids,
        );
    }));
}

enum CallbackError {
    ToI64,
    ParseFlags,
}

fn callback_impl(
    _stream_ref: fs::FSEventStreamRef,
    info: *mut c_void,
    num_events: usize,                               // size_t numEvents
    event_paths: *mut c_void,                        // void *eventPaths
    event_flags: *const fs::FSEventStreamEventFlags, // const FSEventStreamEventFlags eventFlags[]
    event_ids: *const fs::FSEventStreamEventId,      // const FSEventStreamEventId eventIds[]
) {
    debug!("Received {} event(s)", num_events);

    let event_paths = unsafe { CFArray::<CFDictionary<CFString>>::from_void(event_paths) };
    let info = info as *const StreamContextInfo;
    let event_handler = unsafe { &(*info).event_handler };

    for idx in 0..num_events {
        match Ok((
            unsafe { event_paths.get_unchecked(idx as CFIndex) },
            unsafe { *event_flags.add(idx) },
            unsafe { *event_ids.add(idx) },
        ))
        .and_then(|(extended, raw_flags, id)| {
            let path = unsafe {
                CFString::from_void(*extended.get(&*kFSEventStreamEventExtendedDataPathKey))
            };
            let inode = unsafe {
                CFNumber::from_void(*extended.get(&*kFSEventStreamEventExtendedFileIDKey))
            };
            Ok((
                PathBuf::from((*path).to_string()),
                inode.to_i64().ok_or(CallbackError::ToI64)?,
                raw_flags,
                id,
            ))
        })
        .and_then(|(path, inode, raw_flags, id)| {
            StreamFlags::from_bits(raw_flags)
                .ok_or(CallbackError::ParseFlags)
                .map(|flags| RawEvent {
                    path,
                    inode,
                    flags,
                    raw_flags,
                    id,
                })
        }) {
            Ok(raw_event) =>
            // Send event out.
            {
                if let Err(e) = event_handler.try_send(raw_event) {
                    error!("Unable to raw event from low-level callback: {}", e);
                }
            }
            Err(CallbackError::ToI64) => error!("Unable to convert inode field to i64"),
            Err(CallbackError::ParseFlags) => error!("Unable to parse flags"),
        }
    }
}

#[cfg(test)]
mod test {
    use crate::fsevent::StreamContextInfo;

    #[test]
    fn test_steam_context_info_send_and_sync() {
        fn check_send<T: Send + Sync>() {}
        check_send::<StreamContextInfo>();
    }
}
