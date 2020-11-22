use futures::future::{BoxFuture, FutureExt};
use futures::prelude::*;
use iou::{IoUring, SQE};
use mio::{event::Source, net::TcpListener, net::TcpStream};
use nix::sys::socket::SockFlag;
use socket2::{Domain, Socket, Type};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use std::{env, thread};

struct Poller {
    ring: IoUring,
}

thread_local! {
    static POLLER : RefCell<Poller> = {
        RefCell::new(Poller{
            ring: IoUring::new(4096).unwrap(),
        })
    };

    static RUNNABLE : RefCell<VecDeque<Rc<Task>>> = {
        RefCell::new(VecDeque::new())
    };

}

pub fn run<F, T>(f: F)
where
    F: Fn() -> T,
    T: Future<Output = ()> + Send + 'static,
{
    let cpus = {
        env::var("RUSTMAXPROCS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(num_cpus::get)
    };

    println!("Hello, greeter ({} cpus)!", cpus);

    let mut handles = Vec::new();
    for i in 0..cpus {
        let r = f();
        let h = thread::spawn(move || {
            core_affinity::set_for_current(core_affinity::CoreId { id: i });
            spawn(r);

            loop {
                let mut ready = VecDeque::new();
                RUNNABLE.with(|runnable| {
                    ready.append(&mut runnable.borrow_mut());
                });

                while let Some(t) = ready.pop_front() {
                    let future = t.future.borrow_mut();
                    let w = waker(t.clone());
                    let mut context = Context::from_waker(&w);
                    let _ = Pin::new(future).as_mut().poll(&mut context);
                }

                POLLER.with(|poller| {
                    let mut p = poller.borrow_mut();
                    let _ = p.ring.wait_for_cqes(1);

                    while let Some(cqe) = p.ring.peek_for_cqe() {
                        let result = cqe.result();
                        let user_data = cqe.user_data();
                        // println!("cqe done {:?} {:?}", result, user_data);
                        let state = user_data as *mut State;
                        unsafe {
                            let completion = Completion {
                                state: ManuallyDrop::new(Box::from_raw(state)),
                            };
                            completion.complete(result);
                        }
                    }
                });
            }
        });
        handles.push(h);
    }

    for h in handles {
        let _ = h.join();
    }
}

struct Task {
    future: RefCell<BoxFuture<'static, ()>>,
}

impl RcWake for Task {
    fn wake_by_ref(arc_self: &Rc<Self>) {
        RUNNABLE.with(|runnable| runnable.borrow_mut().push_back(arc_self.clone()));
    }
}

pub fn spawn(future: impl Future<Output = ()> + Send + 'static) {
    let t = Rc::new(Task {
        future: RefCell::new(future.boxed()),
    });
    let mut future = t.future.borrow_mut();
    let w = waker(t.clone());
    let mut context = Context::from_waker(&w);
    let _ = future.as_mut().poll(&mut context);
}

pub struct Async<T: Source> {
    io: Box<T>,
    state: AsyncState,
}

#[derive(Debug)]
enum AsyncState {
    Empty,
    Submitted(Completion),
}

impl Async<TcpListener> {
    pub fn new(addr: SocketAddr) -> Self {
        let sock = Socket::new(Domain::ipv6(), Type::stream(), None).unwrap();
        sock.set_reuse_address(true).unwrap();
        sock.set_reuse_port(true).unwrap();
        sock.bind(&addr.into()).unwrap();
        sock.listen(4096).unwrap();

        let listener = TcpListener::from_std(sock.into_tcp_listener());
        Async {
            io: Box::new(listener),
            state: AsyncState::Empty,
        }
    }
}

impl Stream for Async<TcpListener> {
    type Item = std::io::Result<Async<TcpStream>>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        match self.state {
            AsyncState::Empty => {
                let c = Completion::new(cx.waker().clone());
                POLLER.with(|poller| {
                    let mut poller = poller.borrow_mut();
                    let mut q = poller.ring.prepare_sqe().unwrap();
                    unsafe {
                        q.prep_accept(self.io.as_raw_fd(), None, SockFlag::empty());
                        q.set_user_data(c.addr());
                    }
                    let _ = poller.ring.submit_sqes();
                });
                self.state = AsyncState::Submitted(c);
                return std::task::Poll::Pending;
            }
            _ => {}
        }

        match mem::replace(&mut self.state, AsyncState::Empty) {
            AsyncState::Submitted(c) => match *ManuallyDrop::into_inner(c.state) {
                State::Completed(r) => match r {
                    Ok(x) => {
                        return std::task::Poll::Ready(Some(Ok(Async::<TcpStream>::new(unsafe {
                            TcpStream::from_raw_fd(x as i32)
                        }))));
                    }
                    Err(e) => {
                        return std::task::Poll::Ready(Some(Err(e)));
                    }
                },
                _ => panic!("should be completed"),
            },
            _ => panic!("called twice?"),
        }
    }
}

pub struct ReadFuture<'a>(&'a mut Async<TcpStream>, &'a mut [u8]);

impl<'a> Future for ReadFuture<'a> {
    type Output = std::io::Result<usize>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let me = &mut *self;
        let fd = me.0.io.as_raw_fd() as RawFd;
        let addr = me.1.as_mut_ptr();
        let len = me.1.len();
        if me.0.poll_submit(cx, |q| unsafe {
            uring_sys::io_uring_prep_read(q.raw_mut(), fd, addr as _, len as _, 0);
        }) {
            return std::task::Poll::Pending;
        }
        me.0.poll_finish()
    }
}

pub struct WriteFuture<'a>(&'a mut Async<TcpStream>, &'a [u8]);

impl<'a> Future for WriteFuture<'a> {
    type Output = std::io::Result<usize>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let me = &mut *self;
        let fd = me.0.io.as_raw_fd() as RawFd;
        let buf = me.1;
        if me.0.poll_submit(cx, |q| unsafe {
            uring_sys::io_uring_prep_write(q.raw_mut(), fd, buf.as_ptr() as _, buf.len() as _, 0);
        }) {
            return std::task::Poll::Pending;
        }
        me.0.poll_finish()
    }
}

impl Async<TcpStream> {
    pub fn new(io: TcpStream) -> Self {
        Async {
            io: Box::new(io),
            state: AsyncState::Empty,
        }
    }

    fn poll_submit<'a>(&'a mut self, cx: &mut Context<'_>, f: impl FnOnce(&mut SQE<'_>)) -> bool {
        match self.state {
            AsyncState::Empty => {
                let c = Completion::new(cx.waker().clone());
                POLLER.with(|poller| {
                    let mut poller = poller.borrow_mut();
                    let mut q = poller.ring.prepare_sqe().unwrap();
                    f(&mut q);
                    unsafe {
                        q.set_user_data(c.addr());
                    }
                    let _ = poller.ring.submit_sqes();
                });
                self.state = AsyncState::Submitted(c);
                return true;
            }
            _ => return false,
        }
    }

    fn poll_finish<'a>(&'a mut self) -> Poll<std::io::Result<usize>> {
        match mem::replace(&mut self.state, AsyncState::Empty) {
            AsyncState::Submitted(c) => match *ManuallyDrop::into_inner(c.state) {
                State::Completed(r) => match r {
                    Ok(x) => {
                        return std::task::Poll::Ready(Ok(x as usize));
                    }
                    Err(e) => {
                        return std::task::Poll::Ready(Err(e));
                    }
                },
                _ => panic!("should be completed"),
            },
            _ => panic!("called more than twice?"),
        }
    }

    pub fn read<'a>(&'a mut self, b: &'a mut [u8]) -> ReadFuture {
        ReadFuture(self, b)
    }

    pub fn write<'a>(&'a mut self, b: &'a [u8]) -> WriteFuture {
        WriteFuture(self, b)
    }
}

// stolen from the future code
use std::mem::{self, ManuallyDrop};
use std::rc::Rc;
use std::task::{RawWaker, RawWakerVTable};

pub trait RcWake {
    fn wake(self: Rc<Self>) {
        Self::wake_by_ref(&self)
    }

    fn wake_by_ref(arc_self: &Rc<Self>);
}

fn waker<W>(wake: Rc<W>) -> Waker
where
    W: RcWake,
{
    let ptr = Rc::into_raw(wake) as *const ();
    let vtable = &Helper::<W>::VTABLE;
    unsafe { Waker::from_raw(RawWaker::new(ptr, vtable)) }
}

#[allow(clippy::redundant_clone)] // The clone here isn't actually redundant.
unsafe fn increase_refcount<T: RcWake>(data: *const ()) {
    // Retain Arc, but don't touch refcount by wrapping in ManuallyDrop
    let arc = mem::ManuallyDrop::new(Rc::<T>::from_raw(data as *const T));
    // Now increase refcount, but don't drop new refcount either
    let _arc_clone: mem::ManuallyDrop<_> = arc.clone();
}

struct Helper<F>(F);

impl<T: RcWake> Helper<T> {
    const VTABLE: RawWakerVTable = RawWakerVTable::new(
        Self::clone_waker,
        Self::wake,
        Self::wake_by_ref,
        Self::drop_waker,
    );

    unsafe fn clone_waker(data: *const ()) -> RawWaker {
        increase_refcount::<T>(data);
        let vtable = &Helper::<T>::VTABLE;
        RawWaker::new(data, vtable)
    }

    unsafe fn wake(ptr: *const ()) {
        let rc: Rc<T> = Rc::from_raw(ptr as *const T);
        RcWake::wake(rc);
    }

    unsafe fn wake_by_ref(ptr: *const ()) {
        let arc = ManuallyDrop::new(Rc::<T>::from_raw(ptr as *const T));
        RcWake::wake_by_ref(&arc);
    }

    unsafe fn drop_waker(ptr: *const ()) {
        drop(Rc::from_raw(ptr as *const Task));
    }
}

// stolen from ringbahn code
#[derive(Debug)]
pub struct Completion {
    state: ManuallyDrop<Box<State>>,
}

#[derive(Debug)]
enum State {
    Submitted(Waker),
    Completed(io::Result<u32>),
}

impl Completion {
    pub fn new(waker: Waker) -> Completion {
        Completion {
            state: ManuallyDrop::new(Box::new(State::Submitted(waker))),
        }
    }

    pub fn addr(&self) -> u64 {
        &**self.state as *const State as usize as u64
    }

    fn complete(mut self, result: io::Result<u32>) {
        let s = mem::replace(&mut **self.state, State::Completed(result));
        match s {
            State::Submitted(waker) => {
                waker.wake();
            }
            _ => panic!("complete() is called twice"),
        }
    }
}
