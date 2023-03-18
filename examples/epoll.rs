use std::collections::HashMap;
use std::io::prelude::*;
use std::net::TcpStream;
use std::os::fd::{AsRawFd, RawFd};
use std::{io, net::TcpListener};

static HTTP_BASE: &[u8] = b"HTTP/1.1 200 OK
content-type: text/html
content-length: 6

Hello
";

#[allow(unused_macros)]
macro_rules! sys {
    ($fn: ident ( $($arg: expr), * $(,)* )) => {{
        let res = unsafe { libc::$fn($($arg,) *) };
        if res == -1 {
            Err(std::io::Error::last_os_error())
        }else {
            Ok(res)
        }
    }};
}
fn epoll_create() -> io::Result<RawFd> {
    let fd: i32 = sys!(epoll_create1(0))?;
    if let Ok(flags) = sys!(fcntl(fd, libc::F_GETFD)) {
        let _ = sys!(fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC));
    }
    Ok(fd)
}

fn add_interest(epoll_fd: RawFd, fd: RawFd, mut event: libc::epoll_event) -> io::Result<()> {
    sys!(epoll_ctl(epoll_fd, libc::EPOLL_CTL_ADD, fd, &mut event))?;
    Ok(())
}

fn listener_read_event(key: u64) -> libc::epoll_event {
    libc::epoll_event {
        events: (libc::EPOLLONESHOT | libc::EPOLLIN) as u32,
        u64: key,
    }
}

fn listener_write_event(key: u64) -> libc::epoll_event {
    libc::epoll_event {
        events: (libc::EPOLLONESHOT | libc::EPOLLOUT) as u32,
        u64: key,
    }
}

fn modify_interest(epoll_fd: RawFd, fd: RawFd, mut event: libc::epoll_event) -> io::Result<()> {
    sys!(epoll_ctl(epoll_fd, libc::EPOLL_CTL_MOD, fd, &mut event))?;
    Ok(())
}

fn remove_interest(epoll_fd: RawFd, fd: RawFd) -> io::Result<()> {
    sys!(epoll_ctl(
        epoll_fd,
        libc::EPOLL_CTL_DEL,
        fd,
        std::ptr::null_mut()
    ))?;
    Ok(())
}

fn close(fd: RawFd) {
    let _ = sys!(close(fd));
}

#[derive(Debug)]
struct RequestContext {
    pub stream: TcpStream,
    pub content_length: usize,
    pub buf: Vec<u8>,
}
impl RequestContext {
    fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            content_length: 0,
            buf: vec![],
        }
    }
    fn read_cb(&mut self, key: u64, epoll_fd: RawFd) -> io::Result<()> {
        let mut buf = [0u8; 4096];
        match self.stream.read(&mut buf) {
            Ok(_) => {
                if let Ok(data) = std::str::from_utf8(&buf) {
                    self.pass_and_set_content_length(data);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e),
        };
        self.buf.extend_from_slice(&buf);
        if self.buf.len() >= self.content_length {
            // println!("{}", String::from_utf8(self.buf.clone()).unwrap());
            modify_interest(epoll_fd, self.stream.as_raw_fd(), listener_write_event(key))?
        } else {
            modify_interest(epoll_fd, self.stream.as_raw_fd(), listener_read_event(key))?
        }
        Ok(())
    }
    fn write_cb(&mut self, epoll_fd: RawFd) -> io::Result<()> {
        self.stream.write(HTTP_BASE).unwrap();
        self.stream.shutdown(std::net::Shutdown::Both)?;
        let fd = self.stream.as_raw_fd();
        remove_interest(epoll_fd, fd)?;
        close(fd);
        Ok(())
    }
    fn pass_and_set_content_length(&mut self, data: &str) {
        if data.contains("HTTP") {
            if let Some(content_length) = data
                .lines()
                .find(|line| line.to_lowercase().starts_with("content-length: "))
            {
                if let Some(len) = content_length
                    .to_lowercase()
                    .strip_prefix("content-length: ")
                {
                    self.content_length = len.parse::<usize>().expect("content-length is valid");
                    println!("set content length {} bytes", self.content_length);
                }
            }
        }
    }
}

fn main() -> io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:8000")?;
    listener.set_nonblocking(true)?;
    let listener_fd = listener.as_raw_fd();
    let epoll_fd = epoll_create().expect("can't create epoll qiueue");
    let mut key = 100;
    add_interest(epoll_fd, listener_fd, listener_read_event(key))?;

    let mut events: Vec<libc::epoll_event> = Vec::with_capacity(1024);
    let mut contexts = HashMap::new();
    loop {
        events.clear();
        let res = sys!(epoll_wait(
            epoll_fd,
            events.as_mut_ptr() as *mut libc::epoll_event,
            1024,
            1000 as libc::c_int
        ))
        .unwrap();
        unsafe { events.set_len(res as usize) }

        // println!("request in flight: {}", contexts.len());
        for ev in &events {
            match ev.u64 {
                100 => {
                    match listener.accept() {
                        Ok((stream, addr)) => {
                            stream.set_nonblocking(true)?;
                            println!("new client: {}", addr);
                            key += 1;
                            add_interest(epoll_fd, stream.as_raw_fd(), listener_read_event(key))?;
                            contexts.insert(key, RequestContext::new(stream));
                        }
                        Err(e) => eprintln!("con't adccept: {e}"),
                    }
                    modify_interest(epoll_fd, listener_fd, listener_read_event(100))?;
                }
                key => {
                    let mut to_delete = None;
                    if let Some(context) = contexts.get_mut(&key) {
                        let events: u32 = ev.events;
                        println!("events = {}", events);
                        match events {
                            v if v as i32 & libc::EPOLLIN == libc::EPOLLIN => {
                                context.read_cb(key, epoll_fd)?;
                            }
                            v if v as i32 & libc::EPOLLOUT == libc::EPOLLOUT => {
                                context.write_cb(epoll_fd)?;
                                to_delete = Some(key);
                            }
                            v => println!("unexpected events: {v}"),
                        }
                    }
                    if let Some(key) = to_delete {
                        contexts.remove(&key);
                    }
                }
            }
        }
    }

    // Ok(())
}
