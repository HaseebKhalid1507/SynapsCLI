/*
 * The Synaps kill guard (crates/agent-engine/src/killguard.rs; docs/daemon-mode.md, "Kill guard on
 * tool children"). LD_PRELOADed into the processes Synaps tools start, on Linux with glibc.
 *
 * kill(-1, sig) and killpg(1, sig) fail with EPERM and send nothing. glibc's killpg(1) is kill(-1)
 * through an internal __kill, so both are interposed. Every other call is the raw kill syscall,
 * which is what glibc's own kill and killpg do. No dlsym, no constructor: the object needs only
 * syscall and __errno_location from libc.so.6, at their oldest versions, so it loads into any
 * glibc process, and both functions stay async-signal-safe. No no_new_privs, no seccomp.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <unistd.h>

__attribute__((visibility("default"))) int kill(pid_t pid, int sig) {
    if (pid == -1) {
        errno = EPERM;
        return -1;
    }
    return (int)syscall(SYS_kill, pid, sig);
}

__attribute__((visibility("default"))) int killpg(pid_t pgrp, int sig) {
    if (pgrp == 1) {
        errno = EPERM;
        return -1;
    }
    /* glibc's killpg. */
    if (pgrp < 0) {
        errno = EINVAL;
        return -1;
    }
    return (int)syscall(SYS_kill, -pgrp, sig);
}
