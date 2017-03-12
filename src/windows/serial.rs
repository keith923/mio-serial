//! COM port impl.

use std::io::{self, Read, Write};
use std::convert::AsRef;
use std::time::Duration;
use std::path::Path;
use std::os::windows::prelude::*;

use std::sync::{Mutex, MutexGuard};
use std::fmt;
use std::mem;

use mio;
use mio::windows::{Overlapped, Binding};
use mio::{Ready, Poll, Token, PollOpt, IoVec, Evented, Registration, SetReadiness};

use miow::iocp::CompletionStatus;

use winapi::*;
use kernel32;

use serialport;
use serialport::windows::COMPort;
use serialport::prelude::*;

use windows::from_raw_arc::FromRawArc;

macro_rules! overlapped2arc {
    ($e:expr, $t:ty, $($field:ident).+) => ({
        let offset = offset_of!($t, $($field).+);
        debug_assert!(offset < mem::size_of::<$t>());
        FromRawArc::from_raw(($e as usize - offset) as *mut $t)
    })
}

macro_rules! offset_of {
    ($t:ty, $($field:ident).+) => (
        &(*(0 as *const $t)).$($field).+ as *const _ as usize
    )
}

/// Windows COM port
pub struct Serial {
    imp: Imp,
    registration: Mutex<Option<Registration>>,
}

#[derive(Clone)]
struct Imp {
    inner: FromRawArc<Io>,
}

struct Io {
    read: Overlapped,
    write: Overlapped,
    serial: COMPort,
    inner: Mutex<Inner>,
}

struct Inner {
    binding: Binding,
    readiness: Option<SetReadiness>,
    read: State<Vec<u8>, Vec<u8>>,
    write: State<Vec<u8>, Vec<u8>>
}

enum State<T,U> {
    Empty,
    Pending(T),
    Ready(U),
    Error(io::Error),
}

impl Serial {
    fn inner(&self) -> MutexGuard<Inner> {
        self.imp.inner()
    }
}

impl Imp {
    fn inner(&self) -> MutexGuard<Inner> {
        self.inner.inner.lock().unwrap()
    }

    fn add_readiness(&self, me: &Inner, set: Ready) {
        if let Some(ref i) = me.readiness {
            i.set_readiness(set).expect("Event loop disapeared");
        }
    }
}

impl Inner {
    pub fn registered(&self) -> bool {
        self.readiness.is_some()
    }
}

impl Evented for Serial {
   fn register(&self, poll: &Poll, token: Token, events: Ready, opt: PollOpt) -> io::Result<()> {
       let mut me = self.inner();

       // Register our inner IOCP w/ windows.
       unsafe {me.binding.register_handle(&self.imp.inner.serial, token, poll)?; }

       // Create a Registration/SetRediness set
       let (registration, set_rediness) = Registration::new2();

       // Add 'HUP' to the list of desired events if we're readable.
       let events = if events.is_readable() {
           events | Ready::hup()
       }  else {
           events
       };

       // Register ourselves w/ mio
       registration.register(poll, token, events, opt)?;

       // Finaly place the two structs where they belong.
       me.readiness = Some(set_rediness);
       *self.registration.lock().unwrap() = Some(registration);

       Ok(())
   }

   fn reregister(&self, poll: &Poll, token: Token, events: Ready, opt: PollOpt) -> io::Result<()> {
       unsafe {self.inner().binding.reregister_handle(&self.imp.inner.serial, token, poll)?;}

       // Add 'HUP' to the list of desired events if we're readable.
       let events = if events.is_readable() {
           events | Ready::hup()
       }  else {
           events
       };

       self.registration.lock().unwrap()
            .as_mut().unwrap()
            .reregister(poll, token, events, opt)

   }

   fn deregister(&self, poll: &Poll) -> io::Result<()> {
       unsafe { self.inner().binding.deregister_handle(&self.imp.inner.serial, poll)?; }

       self.registration.lock().unwrap()
            .as_ref().unwrap()
            .deregister(poll)
   }

}

fn send_done(status: &OVERLAPPED_ENTRY) {
    let status = CompletionStatus::from_entry(status);
    // trace!("finished a send {}", status.bytes_transferred());
    let me2 = Imp {
        inner: unsafe { overlapped2arc!(status.overlapped(), Io, write) },
    };
    let mut me = me2.inner();
    me.write = State::Empty;
    me2.add_readiness(&mut me, Ready::writable());
}

fn recv_done(status: &OVERLAPPED_ENTRY) {
    let status = CompletionStatus::from_entry(status);
    // trace!("finished a recv {}", status.bytes_transferred());
    let me2 = Imp {
        inner: unsafe { overlapped2arc!(status.overlapped(), Io, read) },
    };
    let mut me = me2.inner();
    let mut buf = match mem::replace(&mut me.read, State::Empty) {
        State::Pending(buf) => buf,
        _ => unreachable!(),
    };
    unsafe {
        buf.set_len(status.bytes_transferred() as usize);
    }
    me.read = State::Ready(buf);
    me2.add_readiness(&mut me, Ready::readable());
}
