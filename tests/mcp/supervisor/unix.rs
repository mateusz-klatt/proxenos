//! Errors from the operating system while stopping a live child.

#[cfg(target_os = "linux")]
mod platform {
    #![allow(unsafe_code)]

    use super::super::*;
    use libc::{BPF_ABS, BPF_JEQ, BPF_JMP, BPF_K, BPF_LD, BPF_RET, BPF_W};

    /// The test launcher gives its server a kernel policy that refuses kill(2). Only this
    /// thread, the server it launches and their descendants inherit the filter.
    fn server_without_signal_permission() -> Server {
        let (send, receive) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let instruction = |code: u32, jf, k| libc::sock_filter {
                code: code as u16,
                jt: 0,
                jf,
                k,
            };
            let number = std::mem::offset_of!(libc::seccomp_data, nr) as u32;
            let denied = libc::SECCOMP_RET_ERRNO | libc::EPERM as u32;
            let mut filter = [
                instruction(BPF_LD | BPF_W | BPF_ABS, 0, number),
                instruction(BPF_JMP | BPF_JEQ | BPF_K, 1, libc::SYS_kill as u32),
                instruction(BPF_RET | BPF_K, 0, denied),
                instruction(BPF_RET | BPF_K, 0, libc::SECCOMP_RET_ALLOW),
            ];
            let program = libc::sock_fprog {
                len: filter.len() as u16,
                filter: filter.as_mut_ptr(),
            };
            let mode = libc::SECCOMP_MODE_FILTER as libc::c_ulong;
            let filter = &raw const program;
            let zero = 0_usize;
            // SAFETY: prctl copies the complete, initialized filter on this call. Both the
            // program and its instructions stay live. Unused machine-word arguments are zero.
            unsafe {
                assert_eq!(
                    libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1_usize, zero, zero, zero),
                    0
                );
                assert_eq!(
                    libc::prctl(libc::PR_SET_SECCOMP, mode, filter, zero, zero),
                    0
                );
            }
            let _ = send.send(server());
        });
        receive
            .recv_timeout(WAIT)
            .expect("server with a restricted signal policy")
    }

    #[test]
    fn a_denied_stop_is_reported_while_the_reaper_keeps_the_live_childs_files() {
        let directory = Directory::new();
        let temp = directory.0.join("instructions");
        let lock = directory.0.join("child");
        let mut args = launch(json!([
            format!("hold:{}", lock.display()),
            "close-output",
            format!("until-released:{}", lock.display())
        ]));
        args["temp_file"] = json!(temp);
        args["deadline_ms"] = json!(3000);
        let mut server = server_without_signal_permission();
        server.start_call(1, "test_run", args);
        let live = lock_file(&lock);
        let failed = server.response(1);
        assert_eq!(failed["result"]["isError"], true, "{failed}");
        assert!(text(&failed).contains("os error 1"), "{failed}");
        assert!(!is_free(&live), "the kernel refused the stop");
        assert!(temp.exists(), "the CLI still owns its temporary input");
        fs::write(lock.with_extension("release"), []).unwrap();
        wait_for("the reaper did not clean up after the CLI exited", || {
            !temp.exists()
        });
        assert!(is_free(&live));
        assert_eq!(text(&server.call(2, "test_echo", json!({}))), "{}");
    }
}
