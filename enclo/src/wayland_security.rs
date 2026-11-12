use std::{
    os::unix::{
        io::{AsFd, FromRawFd, OwnedFd},
        net::UnixListener,
    },
    path::PathBuf,
};

use wayland_client::{protocol::wl_registry, Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::security_context::v1::client::{
    wp_security_context_manager_v1::{self, WpSecurityContextManagerV1},
    wp_security_context_v1::{self, WpSecurityContextV1},
};

struct SetupState {
    manager: Option<WpSecurityContextManagerV1>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for SetupState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            if interface == "wp_security_context_manager_v1" {
                state.manager = Some(registry.bind(name, version.min(1), qh, ()));
            }
        }
    }
}

impl Dispatch<WpSecurityContextManagerV1, ()> for SetupState {
    fn event(
        _: &mut Self,
        _: &WpSecurityContextManagerV1,
        _event: wp_security_context_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        unreachable!()
    }
}

impl Dispatch<WpSecurityContextV1, ()> for SetupState {
    fn event(
        _: &mut Self,
        _: &WpSecurityContextV1,
        _event: wp_security_context_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        unreachable!()
    }
}

/// Connects to the host compositor, registers a security context bound to
/// `socket_path`, and returns the write end of the close pipe.
/// Dropping the returned fd revokes the security context.
pub fn register_security_context(socket_path: PathBuf, app_id: String) -> anyhow::Result<OwnedFd> {
    let conn = Connection::connect_to_env().map_err(|e| {
        anyhow::anyhow!("failed to connect to Wayland display: {e} (is WAYLAND_DISPLAY set?)")
    })?;

    register_on_connection(conn, socket_path, app_id)
}

fn register_on_connection(
    conn: Connection,
    socket_path: PathBuf,
    app_id: String,
) -> anyhow::Result<OwnedFd> {
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());

    let mut state = SetupState { manager: None };
    queue
        .roundtrip(&mut state)
        .map_err(|e| anyhow::anyhow!("Wayland roundtrip failed: {e}"))?;

    let manager = state.manager.take().ok_or_else(|| {
        anyhow::anyhow!(
            "compositor does not support wp_security_context_manager_v1 — \
         upgrade your compositor (KWin 6+, sway, Hyprland) or disable the wayland module"
        )
    })?;

    // Bound listening socket — the compositor will accept() on its dup of this fd.
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path).map_err(|e| {
        anyhow::anyhow!(
            "failed to bind wayland socket at {}: {e}",
            socket_path.display()
        )
    })?;
    let listen_fd: OwnedFd = listener.into();

    // Pipe: compositor holds the read end and polls it; dropping our write end revokes the context.
    let mut pipe_fds = [0i32; 2];
    if unsafe { libc::pipe2(pipe_fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        anyhow::bail!("pipe() failed: {}", std::io::Error::last_os_error());
    }
    let close_read = unsafe { OwnedFd::from_raw_fd(pipe_fds[0]) };
    let close_write = unsafe { OwnedFd::from_raw_fd(pipe_fds[1]) };

    let security_ctx = manager.create_listener(listen_fd.as_fd(), close_read.as_fd(), &qh, ());
    security_ctx.set_sandbox_engine("enclo".into());
    security_ctx.set_app_id(app_id.clone());
    security_ctx.set_instance_id(app_id);
    security_ctx.commit();

    queue
        .roundtrip(&mut state)
        .map_err(|e| anyhow::anyhow!("Wayland roundtrip failed after commit: {e}"))?;

    // listen_fd and close_read drop here; compositor holds its own dups.
    Ok(close_write)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        os::unix::{io::AsRawFd, net::UnixStream},
    };
    fn word(bytes: &[u8]) -> u32 {
        u32::from_ne_bytes(bytes.try_into().unwrap())
    }
    fn send(stream: &mut UnixStream, object: u32, opcode: u16, body: &[u8]) {
        stream.write_all(&object.to_ne_bytes()).unwrap();
        stream
            .write_all(&(((body.len() as u32 + 8) << 16) | opcode as u32).to_ne_bytes())
            .unwrap();
        stream.write_all(body).unwrap();
    }
    fn string(value: &str) -> Vec<u8> {
        let mut body = ((value.len() + 1) as u32).to_ne_bytes().to_vec();
        body.extend(value.as_bytes());
        body.push(0);
        while body.len() % 4 != 0 {
            body.push(0);
        }
        body
    }
    #[test]
    #[ignore = "binds isolated Unix sockets for a minimal Wayland protocol fixture"]
    fn security_context_passes_fds_and_revokes_without_raw_display_fallback() {
        for supported in [false, true] {
            let (client, mut server) = UnixStream::pair().unwrap();
            server
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let peer = std::thread::spawn(move || {
                let mut fds: Vec<OwnedFd> = vec![];
                let mut manager = 0;
                let mut context = 0;
                let mut syncs = 0;
                let mut committed = false;
                loop {
                    let mut header = [0u8; 8];
                    let mut control = [0usize; 16];
                    let mut iov = libc::iovec {
                        iov_base: header.as_mut_ptr().cast(),
                        iov_len: header.len(),
                    };
                    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
                    msg.msg_iov = &mut iov;
                    msg.msg_iovlen = 1;
                    msg.msg_control = control.as_mut_ptr().cast();
                    msg.msg_controllen = std::mem::size_of_val(&control);
                    assert_eq!(
                        unsafe {
                            libc::recvmsg(
                                server.as_raw_fd(),
                                &mut msg,
                                libc::MSG_WAITALL | libc::MSG_CMSG_CLOEXEC,
                            )
                        },
                        8
                    );
                    unsafe {
                        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
                        while !cmsg.is_null() {
                            if (*cmsg).cmsg_level == libc::SOL_SOCKET
                                && (*cmsg).cmsg_type == libc::SCM_RIGHTS
                            {
                                let n = ((*cmsg).cmsg_len - libc::CMSG_LEN(0) as usize)
                                    / std::mem::size_of::<i32>();
                                for fd in std::slice::from_raw_parts(
                                    libc::CMSG_DATA(cmsg).cast::<i32>(),
                                    n,
                                ) {
                                    fds.push(OwnedFd::from_raw_fd(*fd));
                                }
                            }
                            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
                        }
                    }
                    let object = word(&header[..4]);
                    let size_op = word(&header[4..]);
                    let op = size_op & 65535;
                    let mut body = vec![0u8; (size_op >> 16) as usize - 8];
                    server.read_exact(&mut body).unwrap();
                    if object == 1 && op == 1 {
                        if supported {
                            let mut global = 1u32.to_ne_bytes().to_vec();
                            global.extend(string("wp_security_context_manager_v1"));
                            global.extend(1u32.to_ne_bytes());
                            send(&mut server, word(&body), 0, &global);
                        }
                    } else if object == 1 && op == 0 {
                        syncs += 1;
                        send(&mut server, word(&body), 0, &0u32.to_ne_bytes());
                        send(&mut server, 1, 1, &body);
                        if !supported || syncs == 2 {
                            assert_eq!(committed, supported);
                            let mut rest = Vec::new();
                            server.read_to_end(&mut rest).unwrap();
                            return fds;
                        }
                    } else if manager == 0 {
                        // wl_registry.bind; the dynamic new ID is the last word.
                        manager = word(&body[body.len() - 4..]);
                    } else if object == manager && op == 1 {
                        context = word(&body);
                    } else if object == context {
                        if op == 4 {
                            committed = true;
                        } else if op == 1 {
                            assert_eq!(body, string("enclo"));
                        } else {
                            assert_eq!(body, string("fixture"));
                        }
                    } else {
                        panic!("unexpected request {object}:{op}");
                    }
                }
            });
            let path = std::env::temp_dir().join(format!(
                "ew-{}",
                rand_core::RngCore::next_u64(&mut rand_core::OsRng)
            ));
            let result = register_on_connection(
                Connection::from_socket(client).unwrap(),
                path.clone(),
                "fixture".into(),
            );
            let mut fds = peer.join().unwrap();
            if supported {
                let revoker = result.unwrap();
                assert_eq!(fds.len(), 2);
                let read_end = fds.pop().unwrap();
                let mut poll = libc::pollfd {
                    fd: read_end.as_raw_fd(),
                    events: libc::POLLHUP,
                    revents: 0,
                };
                assert_eq!(unsafe { libc::poll(&mut poll, 1, 0) }, 0);
                drop(revoker);
                assert_eq!(unsafe { libc::poll(&mut poll, 1, 1000) }, 1);
                assert_ne!(poll.revents & libc::POLLHUP, 0);
                std::fs::remove_file(path).unwrap();
            } else {
                assert!(result.unwrap_err().to_string().contains("does not support"));
                assert!(fds.is_empty());
                assert!(!path.exists());
            }
        }
    }
}
