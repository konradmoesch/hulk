use std::{
    collections::HashMap,
    io::{BufWriter, Read, Write},
    mem::size_of,
    os::unix::{io::AsRawFd, net::UnixStream, prelude::RawFd},
    ptr::read,
    slice::from_raw_parts,
    thread::sleep,
    time::{Duration, Instant},
};

use color_eyre::eyre::{bail, Result, WrapErr};
use epoll::{ControlOptions, Event, Events};
use log::{debug, error, info, warn};
use rmp_serde::{encode::write_named, from_slice};

use crate::{
    control_frame::HulaControlFrame,
    idle::{charging_skull, send_idle},
    listener::HulaListener,
    robot_state::{Battery, RobotState},
};

const HULA_SOCKET_PATH: &str = "/tmp/hula";
const HULA_TIMEOUT: Duration = Duration::from_secs(1);
const LOLA_SOCKET_PATH: &str = "/tmp/robocup";
const LOLA_SOCKET_RETRY_COUNT: usize = 60;
const LOLA_SOCKET_RETRY_INTERVAL: Duration = Duration::from_secs(1);

fn wait_for_lola() -> Result<UnixStream> {
    for _ in 0..LOLA_SOCKET_RETRY_COUNT {
        if let Ok(socket) = UnixStream::connect(LOLA_SOCKET_PATH) {
            return Ok(socket);
        }
        info!("Waiting for LoLA socket to become available...");
        sleep(LOLA_SOCKET_RETRY_INTERVAL);
    }
    bail!("stopped after {} retries", LOLA_SOCKET_RETRY_COUNT)
}

pub struct Proxy {
    lola: UnixStream,
    hula: HulaListener,
    epoll_fd: RawFd,
}

impl Proxy {
    pub fn initialize() -> Result<Self> {
        let lola = wait_for_lola().wrap_err("failed to connect to LoLA")?;
        let hula = HulaListener::bind(HULA_SOCKET_PATH)
            .wrap_err_with(|| format!("failed to bind {}", HULA_SOCKET_PATH))?;

        let epoll_fd = epoll::create(false).wrap_err("failed to create epoll file descriptor")?;
        add_to_epoll(epoll_fd, lola.as_raw_fd())
            .wrap_err("failed to register LoLA file descriptor in epoll")?;
        add_to_epoll(epoll_fd, hula.as_raw_fd())
            .wrap_err("failed to register hula file descriptor in epoll")?;

        Ok(Self {
            lola,
            hula,
            epoll_fd,
        })
    }

    pub fn run(mut self) -> Result<()> {
        let proxy_start = Instant::now();
        let mut connections = HashMap::new();
        let mut events = [Event::new(Events::empty(), 0); 16];
        let mut writer = BufWriter::with_capacity(786, self.lola.try_clone()?);
        let mut battery = None;

        debug!("Entering epoll loop...");
        loop {
            let epoll_timeout = -1;
            let num_events = epoll::wait(self.epoll_fd, epoll_timeout, &mut events)
                .wrap_err("failed to wait for epoll")?;
            for event in &events[0..num_events] {
                let notified_fd = event.data as i32;
                if notified_fd == self.lola.as_raw_fd() {
                    handle_lola_event(&mut self.lola, &mut connections, proxy_start, &mut battery)?;
                } else if notified_fd == self.hula.as_raw_fd() {
                    debug!("HuLA Event");
                    register_connection(&mut self.hula, &mut connections, self.epoll_fd)?;
                } else {
                    debug!("Connection Event");
                    handle_connection_event(&mut connections, notified_fd, &mut writer, &battery)?;
                }
            }

            if !connections
                .values()
                .filter_map(|connection| connection.last_time_received)
                .any(|instant| instant.elapsed() < HULA_TIMEOUT)
            {
                send_idle(&mut writer, &battery).wrap_err(
                    "a shadowy flight into the dangerous world of a man who does not exist",
                )?;
            }
        }
    }
}

struct Connection {
    socket: UnixStream,
    last_time_received: Option<Instant>,
}

fn handle_lola_event(
    lola: &mut UnixStream,
    connections: &mut HashMap<RawFd, Connection>,
    proxy_start: Instant,
    battery: &mut Option<Battery>,
) -> Result<()> {
    let since_start = proxy_start.elapsed();
    let mut robot_state = read_lola_message(lola).wrap_err("failed to read lola message")?;
    robot_state.received_at = since_start.as_secs_f32();
    *battery = Some(robot_state.battery);

    if connections.is_empty() {
        return Ok(());
    }
    let state_storage_buffer = unsafe {
        from_raw_parts(
            &robot_state as *const RobotState as *const u8,
            size_of::<RobotState>(),
        )
    };
    connections.retain(|_, connection| {
        if let Err(error) = connection.socket.write_all(state_storage_buffer) {
            error!("Failed to write StateStorage to connection: {error}");
            return false;
        }
        if let Err(error) = connection.socket.flush() {
            error!("Failed to flush connection: {error}");
            return false;
        }
        true
    });
    Ok(())
}

fn read_lola_message(lola: &mut UnixStream) -> Result<RobotState> {
    let mut lola_data = [0; 896];
    lola.read_exact(&mut lola_data)
        .wrap_err("failed to read from LoLA socket")?;
    from_slice(&lola_data).wrap_err("failed to parse MessagePack from LoLA StateMessage")
}

fn register_connection(
    hula: &mut HulaListener,
    connections: &mut HashMap<RawFd, Connection>,
    poll_fd: RawFd,
) -> Result<()> {
    let (connection_stream, _) = hula.accept().wrap_err("failed to accept connection")?;
    let connection_fd = connection_stream.as_raw_fd();
    info!("Accepted connection with file descriptor {connection_fd}");
    if connections
        .insert(
            connection_fd,
            Connection {
                socket: connection_stream,
                last_time_received: None,
            },
        )
        .is_some()
    {
        panic!("connection is already registered");
    }
    add_to_epoll(poll_fd, connection_fd)
        .wrap_err("failed to register connection file descriptor")?;

    Ok(())
}

fn handle_connection_event(
    connections: &mut HashMap<RawFd, Connection>,
    notified_fd: RawFd,
    writer: &mut BufWriter<UnixStream>,
    battery: &Option<Battery>,
) -> Result<()> {
    match connections.get_mut(&notified_fd) {
        Some(connection) => {
            let mut read_buffer = [0; size_of::<HulaControlFrame>()];
            if let Err(error) = connection.socket.read_exact(&mut read_buffer) {
                error!("Failed to read from connection: {}", error);
                info!("Removing connection with file descriptor {}", notified_fd);
                // remove will drop, drop will close, close will EPOLL_CTL_DEL
                connections
                    .remove(&notified_fd)
                    .expect("connection file descriptor has to be registered");
                return Ok(());
            };
            connection.last_time_received = Some(Instant::now());
            // reinterpret the read buffer as a ControlFrame
            let control_frame = unsafe { read(read_buffer.as_ptr() as *const HulaControlFrame) };
            let mut lola_message = control_frame.into_lola(Default::default());
            if let Some(battery) = battery {
                lola_message.skull = charging_skull(battery);
            }
            write_named(writer, &lola_message).wrap_err("failed to serialize control message")?;
            writer
                .flush()
                .wrap_err("failed to flush control data to LoLA")?;
        }
        None => warn!(
            "Connection with file descriptor {} does not exist",
            notified_fd
        ),
    }
    Ok(())
}

fn add_to_epoll(
    poll_file_descriptor: RawFd,
    file_descriptor_to_add: RawFd,
) -> Result<(), systemd::Error> {
    epoll::ctl(
        poll_file_descriptor,
        ControlOptions::EPOLL_CTL_ADD,
        file_descriptor_to_add,
        Event::new(
            Events::EPOLLIN | Events::EPOLLERR | Events::EPOLLHUP,
            file_descriptor_to_add as u64,
        ),
    )
}
