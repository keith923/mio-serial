//! Windows async COM
//!
//! Modeled after mio's windows TCP module.
use std::io::{self, Read, Write};
use std::convert::AsRef;
use std::time::Duration;
use std::path::Path;
use std::os::windows::prelude::*;

use std::sync::{Mutex, MutexGuard};
use std::fmt;
use std::mem;

use mio;
use self::from_raw_arc::FromRawArc;
use mio::windows::{Overlapped, Binding};
use mio::{Ready, Poll, Token, PollOpt, IoVec, Evented};

use miow::iocp::CompletionStatus;

use winapi::*;
use kernel32;

use serialport;
use serialport::windows::COMPort;
use serialport::prelude::*;

pub mod from_raw_arc {
    //! A "Manual Arc" which allows manually frobbing the reference count
    //!
    //! This module contains a copy of the `Arc` found in the standard library,
    //! stripped down to the bare bones of what we actually need. The reason this is
    //! done is for the ability to concretely know the memory layout of the `Inner`
    //! structure of the arc pointer itself (e.g. `ArcInner` in the standard
    //! library).
    //!
    //! We do some unsafe casting from `*mut OVERLAPPED` to a `FromRawArc<T>` to
    //! ensure that data lives for the length of an I/O operation, but this means
    //! that we have to know the layouts of the structures involved. This
    //! representation primarily guarantees that the data, `T` is at the front of
    //! the inner pointer always.
    //!
    //! Note that we're missing out on some various optimizations implemented in the
    //! standard library:
    //!
    //! * The size of `FromRawArc` is actually two words because of the drop flag
    //! * The compiler doesn't understand that the pointer in `FromRawArc` is never
    //!   null, so Option<FromRawArc<T>> is not a nullable pointer.

    use std::ops::Deref;
    use std::mem;
    use std::sync::atomic::{self, AtomicUsize, Ordering};

    pub struct FromRawArc<T> {
        _inner: *mut Inner<T>,
    }

    unsafe impl<T: Sync + Send> Send for FromRawArc<T> { }
    unsafe impl<T: Sync + Send> Sync for FromRawArc<T> { }

    #[repr(C)]
    struct Inner<T> {
        data: T,
        cnt: AtomicUsize,
    }

    impl<T> FromRawArc<T> {
        pub fn new(data: T) -> FromRawArc<T> {
            let x = Box::new(Inner {
                data: data,
                cnt: AtomicUsize::new(1),
            });
            FromRawArc { _inner: unsafe { mem::transmute(x) } }
        }

        pub unsafe fn from_raw(ptr: *mut T) -> FromRawArc<T> {
            // Note that if we could use `mem::transmute` here to get a libstd Arc
            // (guaranteed) then we could just use std::sync::Arc, but this is the
            // crucial reason this currently exists.
            FromRawArc { _inner: ptr as *mut Inner<T> }
        }
    }

    impl<T> Clone for FromRawArc<T> {
        fn clone(&self) -> FromRawArc<T> {
            // Atomic ordering of Relaxed lifted from libstd, but the general idea
            // is that you need synchronization to communicate this increment to
            // another thread, so this itself doesn't need to be synchronized.
            unsafe {
                (*self._inner).cnt.fetch_add(1, Ordering::Relaxed);
            }
            FromRawArc { _inner: self._inner }
        }
    }

    impl<T> Deref for FromRawArc<T> {
        type Target = T;

        fn deref(&self) -> &T {
            unsafe { &(*self._inner).data }
        }
    }

    impl<T> Drop for FromRawArc<T> {
        fn drop(&mut self) {
            unsafe {
                // Atomic orderings lifted from the standard library
                if (*self._inner).cnt.fetch_sub(1, Ordering::Release) != 1 {
                    return
                }
                atomic::fence(Ordering::Acquire);
                drop(mem::transmute::<_, Box<T>>(self._inner));
            }
        }
    }
}

macro_rules! offset_of {
    ($t:ty, $($field:ident).+) => (
        &(*(0 as *const $t)).$($field).+ as *const _ as usize
    )
}

macro_rules! overlapped2arc {
    ($e:expr, $t:ty, $($field:ident).+) => ({
        let offset = offset_of!($t, $($field).+);
        debug_assert!(offset < mem::size_of::<$t>());
        FromRawArc::from_raw(($e as usize - offset) as *mut $t)
    })
}

unsafe fn cancel(socket: &AsRawSocket,
                 overlapped: &Overlapped) -> io::Result<()> {
    let handle = socket.as_raw_socket() as HANDLE;
    let ret = kernel32::CancelIoEx(handle, overlapped.as_mut_ptr());
    if ret == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

unsafe fn no_notify_on_instant_completion(handle: HANDLE) -> io::Result<()> {
    // TODO: move those to winapi
    const FILE_SKIP_COMPLETION_PORT_ON_SUCCESS: UCHAR = 1;
    const FILE_SKIP_SET_EVENT_ON_HANDLE: UCHAR = 2;

    let flags = FILE_SKIP_COMPLETION_PORT_ON_SUCCESS | FILE_SKIP_SET_EVENT_ON_HANDLE;

    let r = kernel32::SetFileCompletionNotificationModes(handle, flags);
    if r == TRUE {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

pub struct Serial {
    /// Serial is closely modeled after the TCP/UDP modules in mio for windows.
    imp: SerialImp,
    registration: Mutex<Option<mio::Registration>>,

}

impl Serial {
    pub fn from_path<T: AsRef<Path>>(path: T, settings: &SerialPortSettings) -> io::Result<Self> {
        COMPort::open(path.as_ref(), settings)
            .map(|port| Serial { 
                registration: Mutex::new(None),
                imp: SerialImp {
                    inner: FromRawArc::new(SerialIo {
                        read: Overlapped::new(read_done),
                        write: Overlapped::new(write_done),
                        serial: port,
                        inner: Mutex::new(SerialInner {
                            iocp: Binding::new(),
                            read: State::Empty,
                            write: State::Empty,
                            instant_notify: false,
                        }),
                    }),
                },
            })
            .map_err(|_| Err(io::Error::last_os_error))
    }


    fn inner(&self) -> MutexGuard<SerialInner> {
        self.imp.inner()
    }

    fn post_register(&self, interest: Ready, me: &mut SerialInner) {
        if interest.is_readable() {
            self.imp.schedule_read(me);
        }

        // At least with epoll, if a socket is registered with an interest in
        // writing and it's immediately writable then a writable event is
        // generated immediately, so do so here.
        if interest.is_writable() {
            if let State::Empty = me.write {
                self.imp.add_readiness(me, Ready::writable());
            }
        }
    }

    pub fn readv(&self, bufs: &mut [&mut IoVec]) -> io::Result<usize> {
        let mut me = self.inner();

        match me.read {
            // Empty == we're not associated yet, and if we're pending then
            // these are both cases where we return "would block"
            State::Empty |
            State::Pending(()) => return Err(wouldblock()),

            // If we got a delayed error as part of a `read_overlapped` below,
            // return that here. Also schedule another read in case it was
            // transient.
            State::Error(_) => {
                let e = match mem::replace(&mut me.read, State::Empty) {
                    State::Error(e) => e,
                    _ => panic!(),
                };
                self.imp.schedule_read(&mut me);
                return Err(e)
            }

            // If we're ready for a read then some previous 0-byte read has
            // completed. In that case the OS's socket buffer has something for
            // us, so we just keep pulling out bytes while we can in the loop
            // below.
            State::Ready(()) => {}
        }

        // TODO: Does WSARecv work on a nonblocking sockets? We ideally want to
        //       call that instead of looping over all the buffers and calling
        //       `recv` on each buffer. I'm not sure though if an overlapped
        //       socket in nonblocking mode would work with that use case,
        //       however, so for now we just call `recv`.

        let mut amt = 0;
        for buf in bufs {
            let buf = buf.as_mut_bytes();

            match (&self.imp.inner.serial).read(buf) {
                // If we did a partial read, then return what we've read so far
                Ok(n) if n < buf.len() => return Ok(amt + n),

                // Otherwise filled this buffer entirely, so try to fill the
                // next one as well.
                Ok(n) => amt += n,

                // If we hit an error then things get tricky if we've already
                // read some data. If the error is "would block" then we just
                // return the data we've read so far while scheduling another
                // 0-byte read.
                //
                // If we've read data and the error kind is not "would block",
                // then we stash away the error to get returned later and return
                // the data that we've read.
                //
                // Finally if we haven't actually read any data we just
                // reschedule a 0-byte read to happen again and then return the
                // error upwards.
                Err(e) => {
                    if amt > 0 && e.kind() == io::ErrorKind::WouldBlock {
                        me.read = State::Empty;
                        self.imp.schedule_read(&mut me);
                        return Ok(amt)
                    } else if amt > 0 {
                        me.read = State::Error(e);
                        return Ok(amt)
                    } else {
                        me.read = State::Empty;
                        self.imp.schedule_read(&mut me);
                        return Err(e)
                    }
                }
            }
        }

        Ok(amt)
    }

    pub fn writev(&self, bufs: &[&IoVec]) -> io::Result<usize> {
        let mut me = self.inner();
        let me = &mut *me;

        match me.write {
            State::Empty => {}
            _ => return Err(wouldblock())
        }

        if !me.iocp.registered() {
            return Err(wouldblock())
        }

        if bufs.len() == 0 {
            return Ok(0)
        }

        let len = bufs.iter().map(|b| b.as_bytes().len()).fold(0, |a, b| a + b);
        let mut intermediate = me.iocp.get_buffer(len);
        for buf in bufs {
            intermediate.extend_from_slice(buf.as_bytes());
        }
        self.imp.schedule_write(intermediate, 0, me);
        Ok(len)
    }
}

impl SerialImp {
    fn inner(&self) -> MutexGuard<SerialInner> {
        self.inner.inner.lock().unwrap()
    }

    fn schedule_read(&self, me: &mut SerialInner) {
        match me.read {
            State::Empty => {}
            State::Ready(_) | State::Error(_) => {
                self.add_readiness(me, Ready::readable());
                return;
            }
            _ => return,
        }

        me.iocp.set_readiness(me.iocp.readiness() & !Ready::readable());

        //trace!("scheduling a read");
        let res = unsafe {
            self.inner.serial.read_overlapped(&mut [], self.inner.read.as_mut_ptr())
        };
        match res {
            // Note that `Ok(true)` means that this completed immediately and
            // our socket is readable. This typically means that the caller of
            // this function (likely `read` above) can try again as an
            // optimization and return bytes quickly.
            //
            // Normally, though, although the read completed immediately
            // there's still an IOCP completion packet enqueued that we're going
            // to receive.
            //
            // You can configure this behavior (miow) with
            // SetFileCompletionNotificationModes to indicate that `Ok(true)`
            // does **not** enqueue a completion packet. (This is the case
            // for me.instant_notify)
            //
            // Note that apparently libuv has scary code to work around bugs in
            // `WSARecv` for UDP sockets apparently for handles which have had
            // the `SetFileCompletionNotificationModes` function called on them,
            // worth looking into!
            Ok(Some(_)) if me.instant_notify => {
                me.read = State::Ready(());
                self.add_readiness(me, Ready::readable());
            }
            Ok(_) => {
                // see docs above on SerialImp.inner for rationale on forget
                me.read = State::Pending(());
                mem::forget(self.clone());
            }
            Err(e) => {
                // Like above, be sure to indicate that hup has happened
                // whenever we get `ECONNRESET`
                let mut set = Ready::readable();
                if e.raw_os_error() == Some(WSAECONNRESET as i32) {
                    //trace!("tcp stream at hup: econnreset");
                    set = set | Ready::hup();
                }
                me.read = State::Error(e);
                self.add_readiness(me, set);
            }
        }
    }

    /// Similar to `schedule_read`, except that this issues, well, writes.
    ///
    /// This function will continually attempt to write the entire contents of
    /// the buffer `buf` until they have all been written. The `pos` argument is
    /// the current offset within the buffer up to which the contents have
    /// already been written.
    ///
    /// A new writable event (e.g. allowing another write) will only happen once
    /// the buffer has been written completely (or hit an error).
    fn schedule_write(&self,
                      buf: Vec<u8>,
                      mut pos: usize,
                      me: &mut SerialInner) {

        // About to write, clear any pending level triggered events
        me.iocp.set_readiness(me.iocp.readiness() & !Ready::writable());

        //trace!("scheduling a write");
        loop {
            let ret = unsafe {
                self.inner.serial.write_overlapped(&buf[pos..], self.inner.write.as_mut_ptr())
            };
            match ret {
                Ok(Some(transferred_bytes)) if me.instant_notify => {
                    if transferred_bytes == buf.len() - pos {
                        self.add_readiness(me, Ready::writable());
                        me.write = State::Empty;
                        break;
                    }
                    pos += transferred_bytes;
                }
                Ok(_) => {
                    // see docs above on SerialImp.inner for rationale on forget
                    me.write = State::Pending((buf, pos));
                    mem::forget(self.clone());
                    break;
                }
                Err(e) => {
                    me.write = State::Error(e);
                    self.add_readiness(me, Ready::writable());
                    me.iocp.put_buffer(buf);
                    break;
                }
            }
        }
    }

    /// Pushes an event for this socket onto the selector its registered for.
    ///
    /// When an event is generated on this socket, if it happened after the
    /// socket was closed then we don't want to actually push the event onto our
    /// selector as otherwise it's just a spurious notification.
    fn add_readiness(&self, me: &mut SerialInner, set: Ready) {
        me.iocp.set_readiness(set | me.iocp.readiness());
    }
}

fn read_done(status: &OVERLAPPED_ENTRY) {
    let status = CompletionStatus::from_entry(status);
    let me2 = SerialImp {
        inner: unsafe { overlapped2arc!(status.overlapped(), SerialIo, read) },
    };

    let mut me = me2.inner();
    match mem::replace(&mut me.read, State::Empty) {
        State::Pending(()) => {
            //trace!("finished a read: {}", status.bytes_transferred());
            assert_eq!(status.bytes_transferred(), 0);
            me.read = State::Ready(());
            return me2.add_readiness(&mut me, Ready::readable())
        }
        s => me.read = s,
    }

    // If a read didn't complete, then the connect must have just finished.
    //trace!("finished a connect");

    match me2.inner.serial.connect_complete() {
        Ok(()) => {
            me2.add_readiness(&mut me, Ready::writable());
            me2.schedule_read(&mut me);
        }
        Err(e) => {
            me2.add_readiness(&mut me, Ready::readable() | Ready::writable());
            me.read = State::Error(e);
        }
    }
}

fn write_done(status: &OVERLAPPED_ENTRY) {
    let status = CompletionStatus::from_entry(status);
    //trace!("finished a write {}", status.bytes_transferred());
    let me2 = SerialImp {
        inner: unsafe { overlapped2arc!(status.overlapped(), SerialIo, write) },
    };
    let mut me = me2.inner();
    let (buf, pos) = match mem::replace(&mut me.write, State::Empty) {
        State::Pending(pair) => pair,
        _ => unreachable!(),
    };
    let new_pos = pos + (status.bytes_transferred() as usize);
    if new_pos == buf.len() {
        me2.add_readiness(&mut me, Ready::writable());
    } else {
        me2.schedule_write(buf, new_pos, &mut me);
    }
}

#[derive(Clone)]
struct SerialImp {
    inner: FromRawArc<SerialIo>,
}

struct SerialIo {
    inner: Mutex<SerialInner>,
    read: Overlapped,
    write: Overlapped,
    serial: COMPort,
}

struct SerialInner {
    iocp: Binding,
    read: State<(), ()>,
    write: State<(Vec<u8>, usize), (Vec<u8>, usize)>,
    instant_notify: bool,
}

enum State<T, U> {
    Empty,
    Pending(T),
    Ready(U),
    Error(io::Error),
}


impl SerialPort for Serial {
    /// Returns a struct with the current port settings
    fn settings(&self) -> SerialPortSettings {
        self.inner.settings()
    }

    /// Return the name associated with the serial port, if known.
    fn port_name(&self) -> Option<String> {
        self.inner.port_name()
    }

    /// Returns the current baud rate.
    ///
    /// This function returns `None` if the baud rate could not be determined. This may occur if
    /// the hardware is in an uninitialized state. Setting a baud rate with `set_baud_rate()`
    /// should initialize the baud rate to a supported value.
    fn baud_rate(&self) -> Option<::BaudRate> {
        self.inner.baud_rate()
    }

    /// Returns the character size.
    ///
    /// This function returns `None` if the character size could not be determined. This may occur
    /// if the hardware is in an uninitialized state or is using a non-standard character size.
    /// Setting a baud rate with `set_char_size()` should initialize the character size to a
    /// supported value.
    fn data_bits(&self) -> Option<::DataBits> {
        self.inner.data_bits()
    }

    /// Returns the flow control mode.
    ///
    /// This function returns `None` if the flow control mode could not be determined. This may
    /// occur if the hardware is in an uninitialized state or is using an unsupported flow control
    /// mode. Setting a flow control mode with `set_flow_control()` should initialize the flow
    /// control mode to a supported value.
    fn flow_control(&self) -> Option<::FlowControl> {
        self.inner.flow_control()
    }

    /// Returns the parity-checking mode.
    ///
    /// This function returns `None` if the parity mode could not be determined. This may occur if
    /// the hardware is in an uninitialized state or is using a non-standard parity mode. Setting
    /// a parity mode with `set_parity()` should initialize the parity mode to a supported value.
    fn parity(&self) -> Option<::Parity> {
        self.inner.parity()
    }

    /// Returns the number of stop bits.
    ///
    /// This function returns `None` if the number of stop bits could not be determined. This may
    /// occur if the hardware is in an uninitialized state or is using an unsupported stop bit
    /// configuration. Setting the number of stop bits with `set_stop-bits()` should initialize the
    /// stop bits to a supported value.
    fn stop_bits(&self) -> Option<::StopBits> {
        self.inner.stop_bits()
    }

    /// Returns the current timeout.
    fn timeout(&self) -> Duration {
        Duration::from_secs(0)
    }

    // Port settings setters

    /// Applies all settings for a struct. This isn't guaranteed to involve only
    /// a single call into the driver, though that may be done on some
    /// platforms.
    fn set_all(&mut self, settings: &SerialPortSettings) -> serialport::Result<()> {
        self.inner.set_all(settings)
    }

    /// Sets the baud rate.
    ///
    /// ## Errors
    ///
    /// If the implementation does not support the requested baud rate, this function may return an
    /// `InvalidInput` error. Even if the baud rate is accepted by `set_baud_rate()`, it may not be
    /// supported by the underlying hardware.
    fn set_baud_rate(&mut self, baud_rate: ::BaudRate) -> serialport::Result<()> {
        self.inner.set_baud_rate(baud_rate)
    }

    /// Sets the character size.
    fn set_data_bits(&mut self, data_bits: ::DataBits) -> serialport::Result<()> {
        self.inner.set_data_bits(data_bits)
    }

    /// Sets the flow control mode.
    fn set_flow_control(&mut self, flow_control: ::FlowControl) -> serialport::Result<()> {
        self.inner.set_flow_control(flow_control)
    }

    /// Sets the parity-checking mode.
    fn set_parity(&mut self, parity: ::Parity) -> serialport::Result<()> {
        self.inner.set_parity(parity)
    }

    /// Sets the number of stop bits.
    fn set_stop_bits(&mut self, stop_bits: ::StopBits) -> serialport::Result<()> {
        self.inner.set_stop_bits(stop_bits)
    }

    /// Sets the timeout for future I/O operations.  This parameter is ignored but
    /// required for trait completeness.
    fn set_timeout(&mut self, _: Duration) -> serialport::Result<()> {
        Ok(())
    }

    // Functions for setting non-data control signal pins

    /// Sets the state of the RTS (Request To Send) control signal.
    ///
    /// Setting a value of `true` asserts the RTS control signal. `false` clears the signal.
    ///
    /// ## Errors
    ///
    /// This function returns an error if the RTS control signal could not be set to the desired
    /// state on the underlying hardware:
    ///
    /// * `NoDevice` if the device was disconnected.
    /// * `Io` for any other type of I/O error.
    fn write_request_to_send(&mut self, level: bool) -> serialport::Result<()> {
        self.inner.write_request_to_send(level)
    }

    /// Writes to the Data Terminal Ready pin
    ///
    /// Setting a value of `true` asserts the DTR control signal. `false` clears the signal.
    ///
    /// ## Errors
    ///
    /// This function returns an error if the DTR control signal could not be set to the desired
    /// state on the underlying hardware:
    ///
    /// * `NoDevice` if the device was disconnected.
    /// * `Io` for any other type of I/O error.
    fn write_data_terminal_ready(&mut self, level: bool) -> serialport::Result<()> {
        self.inner.write_data_terminal_ready(level)
    }

    // Functions for reading additional pins

    /// Reads the state of the CTS (Clear To Send) control signal.
    ///
    /// This function returns a boolean that indicates whether the CTS control signal is asserted.
    ///
    /// ## Errors
    ///
    /// This function returns an error if the state of the CTS control signal could not be read
    /// from the underlying hardware:
    ///
    /// * `NoDevice` if the device was disconnected.
    /// * `Io` for any other type of I/O error.
    fn read_clear_to_send(&mut self) -> serialport::Result<bool> {
        self.inner.read_clear_to_send()
    }

    /// Reads the state of the Data Set Ready control signal.
    ///
    /// This function returns a boolean that indicates whether the DSR control signal is asserted.
    ///
    /// ## Errors
    ///
    /// This function returns an error if the state of the DSR control signal could not be read
    /// from the underlying hardware:
    ///
    /// * `NoDevice` if the device was disconnected.
    /// * `Io` for any other type of I/O error.
    fn read_data_set_ready(&mut self) -> serialport::Result<bool> {
        self.inner.read_data_set_ready()
    }

    /// Reads the state of the Ring Indicator control signal.
    ///
    /// This function returns a boolean that indicates whether the RI control signal is asserted.
    ///
    /// ## Errors
    ///
    /// This function returns an error if the state of the RI control signal could not be read from
    /// the underlying hardware:
    ///
    /// * `NoDevice` if the device was disconnected.
    /// * `Io` for any other type of I/O error.
    fn read_ring_indicator(&mut self) -> serialport::Result<bool> {
        self.inner.read_ring_indicator()
    }

    /// Reads the state of the Carrier Detect control signal.
    ///
    /// This function returns a boolean that indicates whether the CD control signal is asserted.
    ///
    /// ## Errors
    ///
    /// This function returns an error if the state of the CD control signal could not be read from
    /// the underlying hardware:
    ///
    /// * `NoDevice` if the device was disconnected.
    /// * `Io` for any other type of I/O error.
    fn read_carrier_detect(&mut self) -> serialport::Result<bool> {
        self.inner.read_carrier_detect()
    }
}

impl AsRawHandle for Serial {
    fn as_raw_handle(&self) -> RawHandle {
        self.inner.as_raw_handle()
    }
}

impl Read for Serial {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.handle.read(bytes)
    }
}

impl Evented for Serial {
    fn register(&self, poll: &Poll, token: Token,
                interest: Ready, opts: PollOpt) -> io::Result<()> {
        let mut me = self.inner();
        try!(me.iocp.register_socket(&self.imp.inner.serial, poll, token,
                                     interest, opts, &self.registration));

        unsafe {
            try!(no_notify_on_instant_completion(self.imp.inner.serial.as_raw_socket() as HANDLE));
            me.instant_notify = true;
        }

        // If we were connected before being registered process that request
        // here and go along our merry ways. Note that the callback for a
        // successful connect will worry about generating writable/readable
        // events and scheduling a new read.
        if let Some(addr) = me.deferred_connect.take() {
            return self.imp.schedule_connect(&addr).map(|_| ())
        }
        self.post_register(interest, &mut me);
        Ok(())
    }

    fn reregister(&self, poll: &Poll, token: Token,
                  interest: Ready, opts: PollOpt) -> io::Result<()> {
        let mut me = self.inner();
        try!(me.iocp.reregister_socket(&self.imp.inner.serial, poll, token,
                                       interest, opts, &self.registration));
        self.post_register(interest, &mut me);
        Ok(())
    }

    fn deregister(&self, poll: &Poll) -> io::Result<()> {
        self.inner().iocp.deregister(&self.imp.inner.serial,
                                     poll, &self.registration)
    }
}

impl fmt::Debug for Serial {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        "TcpStream { ... }".fmt(f)
    }
}

impl Drop for Serial {
    fn drop(&mut self) {
        // If we're still internally reading, we're no longer interested. Note
        // though that we don't cancel any writes which may have been issued to
        // preserve the same semantics as Unix.
        //
        // Note that "Empty" here may mean that a connect is pending, so we
        // cancel even if that happens as well.
        unsafe {
            match self.inner().read {
                State::Pending(_) | State::Empty => {
                    //trace!("cancelling active Serial read");
                    drop(cancel(&self.imp.inner.serial,
                                       &self.imp.inner.read));
                }
                State::Ready(_) | State::Error(_) => {}
            }
        }
    }
}

// TODO: Use std's allocation free io::Error
const WOULDBLOCK: i32 = winerror::WSAEWOULDBLOCK as i32;

/// Returns a std `WouldBlock` error without allocating
pub fn would_block() -> ::std::io::Error {
    ::std::io::Error::from_raw_os_error(WOULDBLOCK)
}

fn wouldblock() -> io::Error {
    io::Error::new(io::ErrorKind::WouldBlock, "operation would block")
}
