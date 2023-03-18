#[macro_use]
extern crate lazy_static;
use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    os::fd::{AsRawFd, RawFd},
    sync::{
        mpsc::{channel, Sender},
        Mutex,
    },
};

use rand::random;

macro_rules! sys {
    ( $fn: ident ($ ($arg: expr), * $(,)* ) ) => {{
        let res = unsafe{ libc::$fn($($arg,)*)};
        if res == -1 {
            Err(std::io::Error::last_os_error())
        }else{
            Ok(res)
        }
    }};
}

type EventId = usize;

struct Poll {
    epoll_fd: RawFd,
}
impl Poll {
    fn new() -> Self {
        let epoll_fd = sys!(epoll_create1(0)).unwrap();
        if let Ok(flags) = sys!(fcntl(epoll_fd, libc::F_GETFD)) {
            let _ = sys!(fcntl(epoll_fd, libc::F_SETFD, flags | libc::FD_CLOEXEC));
        }
        Self { epoll_fd }
    }
    fn poll(&self, events: &mut Vec<libc::epoll_event>) {
        events.clear();
        let len = sys!(epoll_wait(
            self.epoll_fd,
            events.as_mut_ptr() as *mut libc::epoll_event,
            1024,
            1000
        ))
        .unwrap();
        unsafe { events.set_len(len as usize) };
    }
    pub fn registry(&self) -> Registry {
        Registry::new(self.epoll_fd)
    }
}

#[derive(PartialEq, Eq, Hash)]
enum Interest {
    READ,
    WRITE,
}

struct Registry {
    epoll_fd: RawFd,
    io_sources: HashMap<RawFd, HashSet<Interest>>,
}

impl Registry {
    pub fn new(epoll_fd: RawFd) -> Self {
        Self {
            epoll_fd,
            io_sources: HashMap::new(),
        }
    }

    fn registry(
        &mut self,
        fd: RawFd,
        ins: Interest,
        event: &mut libc::epoll_event,
    ) -> io::Result<()> {
        let interests = self.io_sources.entry(fd).or_insert(HashSet::new());
        if interests.is_empty() {
            sys!(epoll_ctl(self.epoll_fd, libc::EPOLL_CTL_ADD, fd, event))?;
        } else {
            sys!(epoll_ctl(self.epoll_fd, libc::EPOLL_CTL_MOD, fd, event))?;
        }
        interests.clear();
        interests.insert(ins);
        Ok(())
    }

    pub fn register_read(&mut self, fd: RawFd, event_id: EventId) -> io::Result<()> {
        self.registry(fd, Interest::READ, &mut read_event(event_id))
    }
    pub fn register_write(&mut self, fd: RawFd, event_id: EventId) -> io::Result<()> {
        self.registry(fd, Interest::WRITE, &mut write_event(event_id))
    }
    pub fn register_remove(&mut self, fd: RawFd) -> io::Result<()> {
        self.io_sources.remove(&fd);
        sys!(epoll_ctl(
            self.epoll_fd,
            libc::EPOLL_CTL_DEL,
            fd,
            std::ptr::null_mut()
        ))?;
        sys!(close(fd)).unwrap();
        Ok(())
    }
}

static EVENT_IN: i32 = libc::EPOLLONESHOT | libc::EPOLLIN;
static EVENT_OUT: i32 = libc::EPOLLONESHOT | libc::EPOLLOUT;

fn read_event(id: EventId) -> libc::epoll_event {
    libc::epoll_event {
        events: EVENT_IN as u32,
        u64: id as u64,
    }
}
fn write_event(id: EventId) -> libc::epoll_event {
    libc::epoll_event {
        events: EVENT_OUT as u32,
        u64: id as u64,
    }
}

struct Reactor {
    pub registry: Option<Registry>,
}

impl Reactor {
    pub fn new() -> Self {
        Self { registry: None }
    }
    pub fn run(&mut self, sender: Sender<EventId>) {
        let poller = Poll::new();
        let registry = poller.registry();

        self.registry = Some(registry);
        // let poller = Arc::new(Box::new(poller));
        std::thread::spawn(move || {
            let mut events: Vec<libc::epoll_event> = Vec::with_capacity(1024);
            loop {
                poller.poll(&mut events);
                for ev in &events {
                    sender.send(ev.u64 as EventId).unwrap();
                }
            }
        });
    }
    pub fn read_interest(&mut self, fd: RawFd, id: EventId) -> io::Result<()> {
        self.registry.as_mut().unwrap().register_read(fd, id)
    }
    pub fn write_interest(&mut self, fd: RawFd, id: EventId) -> io::Result<()> {
        self.registry.as_mut().unwrap().register_write(fd, id)
    }
    pub fn close(&mut self, fd: RawFd) -> io::Result<()> {
        self.registry.as_mut().unwrap().register_remove(fd)
    }
}

struct Exector {
    event_map: HashMap<EventId, Box<dyn FnMut(&mut Self) + Sync + Send + 'static>>,
    event_map_once: HashMap<EventId, Box<dyn FnOnce(&mut Self) + Sync + Send + 'static>>,
}

// impl Debug for Exector {}

impl Debug for Exector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = format!("");
        for (key, _) in self.event_map.iter() {
            s = format!("{} ({}={})", s, key, "FnMut");
        }
        let mut ss = format!("");
        for (key, _) in self.event_map_once.iter() {
            ss = format!("{} ({}={})", ss, key, "FnOnce");
        }
        write!(
            f,
            "Exector: \n\tevent_map: {} \n\tevent_map_once: {} ",
            s, ss
        )
    }
}

impl Exector {
    pub fn new() -> Self {
        Self {
            event_map: HashMap::new(),
            event_map_once: HashMap::new(),
        }
    }
    pub fn once(&mut self, id: EventId, fun: impl FnOnce(&mut Self) + Sync + Send + 'static) {
        self.event_map_once.insert(id, Box::new(fun));
    }
    pub fn keep(&mut self, id: EventId, fun: impl FnMut(&mut Self) + Sync + Send + 'static) {
        self.event_map.insert(id, Box::new(fun));
    }

    pub fn run(&mut self, id: EventId) {
        if let Some(mut fun) = self.event_map.remove(&id) {
            fun(self);
            self.event_map.insert(id, fun);
        } else if let Some(fun) = self.event_map_once.remove(&id) {
            fun(self);
        }
    }
}

lazy_static! {
    static ref EXECTOR: Mutex<Exector> = Mutex::new(Exector::new());
    static ref REACTOR: Mutex<Reactor> = Mutex::new(Reactor::new());
    static ref CONTEXTS: Mutex<HashMap<EventId, Context>> = Mutex::new(HashMap::new());
}

#[derive(Debug)]
struct Context {
    stream: TcpStream,
    length: usize,
    buffer: Vec<u8>,
}
impl Context {
    pub fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            length: 0,
            buffer: vec![],
        }
    }
    pub fn read_cb(&mut self, id: EventId, exec: &mut Exector) -> io::Result<()> {
        let mut buf = [0u8; 4096];
        match self.stream.read(&mut buf) {
            Ok(_) => {
                if let Ok(data) = std::str::from_utf8(&buf) {
                    self.set_content(data);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e),
        }
        self.buffer.extend_from_slice(&buf);
        if self.buffer.len() >= self.length {
            println!("read data: {} bytes", self.buffer.len());
            REACTOR
                .lock()
                .unwrap()
                .write_interest(self.stream.as_raw_fd(), id)
                .unwrap();
            write_cb(exec, id);
        } else {
            REACTOR
                .lock()
                .unwrap()
                .read_interest(self.stream.as_raw_fd(), id)
                .unwrap();
            read_cb(exec, id);
        }
        Ok(())
    }
    pub fn set_content(&mut self, data: &str) {
        if data.contains("HTTP") {
            if let Some(content_lenght) = data
                .lines()
                .find(|line| line.to_lowercase().starts_with("content-length: "))
            {
                if let Some(len) = content_lenght
                    .to_lowercase()
                    .strip_prefix("content-length: ")
                {
                    self.length = len.parse::<usize>().unwrap();
                }
            }
        }
    }
    fn write_cb(&mut self) -> io::Result<()> {
        self.stream.write(HTTP_BASE).unwrap();
        self.stream.shutdown(std::net::Shutdown::Both).unwrap();
        println!("shutdown");
        REACTOR
            .lock()
            .unwrap()
            .close(self.stream.as_raw_fd())
            .unwrap();
        Ok(())
    }
}

fn read_cb(exec: &mut Exector, id: EventId) {
    exec.once(id, move |write_cb| {
        if let Some(ctx) = CONTEXTS.lock().unwrap().get_mut(&id) {
            ctx.read_cb(id, write_cb).unwrap();
        }
    })
}
fn write_cb(exec: &mut Exector, id: EventId) {
    exec.once(id, move |_| {
        if let Some(ctx) = CONTEXTS.lock().unwrap().get_mut(&id) {
            ctx.write_cb().unwrap();
        }
        CONTEXTS.lock().unwrap().remove(&id);
    });
}

const HTTP_BASE: &[u8] = b"HTTP/1.1 200 OK
content-length: 6
content-type: text/html

Hello
";

fn main() -> io::Result<()> {
    let listener_id = 0;
    let listener = TcpListener::bind("127.0.0.1:8000")?;
    listener.set_nonblocking(true).unwrap();

    let listener_fd = listener.as_raw_fd();

    let (sender, receiver) = channel();

    REACTOR.lock().unwrap().run(sender);
    REACTOR
        .lock()
        .unwrap()
        .read_interest(listener_fd, listener_id)?;

    {
        EXECTOR.lock().unwrap().keep(listener_id, move |exec| {
            if let Ok((stream, addr)) = listener.accept() {
                let id: EventId = random();
                println!("accpet: {addr}");
                stream.set_nonblocking(true).unwrap();
                REACTOR
                    .lock()
                    .unwrap()
                    .read_interest(stream.as_raw_fd(), id)
                    .unwrap();
                CONTEXTS.lock().unwrap().insert(id, Context::new(stream));
                read_cb(exec, id);
            }
            REACTOR
                .lock()
                .unwrap()
                .read_interest(listener_fd, listener_id)
                .unwrap();
        });
    }
    while let Ok(id) = receiver.recv() {
        EXECTOR.lock().unwrap().run(id);
    }

    Ok(())
}
