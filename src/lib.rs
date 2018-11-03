//! A library to help grow the stack when it runs out of space.
//!
//! This is an implementation of manually instrumented segmented stacks where
//! points in a program's control flow are annotated with "maybe grow the stack
//! here". Each point of annotation indicates how far away from the end of the
//! stack it's allowed to be, plus the amount of stack to allocate if it does
//! reach the end.
//!
//! Once a program has reached the end of its stack, a temporary stack on the
//! heap is allocated and is switched to for the duration of a closure.
//!
//! # Examples
//!
//! ```
//! // Grow the stack if we are within the "red zone" of 32K, and if we allocate
//! // a new stack allocate 1MB of stack space.
//! //
//! // If we're already in bounds, however, just run the provided closure on our
//! // own stack
//! stacker::maybe_grow(32 * 1024, 1024 * 1024, || {
//!     // guaranteed to have at least 32K of stack
//! });
//! ```
//!
//! # Platform support
//!
//! Only Windows, MacOS and Linux are supported. Other platforms don't do anything
//! and will overflow your stack.

#![allow(improper_ctypes)]

#[macro_use]
extern crate cfg_if;
extern crate libc;

use std::cell::Cell;

extern {
    fn __stacker_stack_pointer() -> usize;
    fn __stacker_switch_stacks(new_stack: usize,
                               fnptr: *const u8,
                               dataptr: *mut u8);
}

thread_local! {
    static STACK_LIMIT: Cell<Option<usize>> = Cell::new(unsafe {
        guess_os_stack_limit()
    })
}

fn get_stack_limit() -> Option<usize> {
    STACK_LIMIT.with(|s| s.get())
}

fn set_stack_limit(l: usize) {
    STACK_LIMIT.with(|s| s.set(Some(l)))
}

/// Grows the call stack if necessary.
///
/// This function is intended to be called at manually instrumented points in a
/// program where recursion is known to happen quite a bit. This function will
/// check to see if we're within `red_zone` bytes of the end of the stack, and
/// if so it will allocate a new stack of size `stack_size`.
///
/// The closure `f` is guaranteed to run on a stack with at least `red_zone`
/// bytes, and it will be run on the current stack if there's space available.
pub fn maybe_grow<R, F: FnOnce() -> R>(red_zone: usize, stack_size: usize, f: F) -> R {
    if let Some(remaining_stack_bytes) = remaining_stack() {
        if remaining_stack_bytes >= red_zone {
            f()
        } else {
            grow_the_stack(stack_size, f, remaining_stack_bytes)
        }
    } else {
        f()
    }
}

/// Queries the amount of remaining stack as interpreted by this library.
///
/// This function will return the amount of stack space left which will be used
/// to determine whether a stack switch should be made or not.
pub fn remaining_stack() -> Option<usize> {
    get_stack_limit().map(|limit| unsafe {
        __stacker_stack_pointer() - limit
    })
}

#[inline(never)]
fn grow_the_stack<R, F: FnOnce() -> R>(stack_size: usize, f: F, remaining_stack_bytes: usize) -> R {
    let mut f = Some(f);
    let mut ret = None;
    unsafe {
        _grow_the_stack(stack_size, remaining_stack_bytes, &mut || {
            let f: F = f.take().unwrap();
            ret = Some(std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)));
        });
    }
    match ret.unwrap() {
        Ok(ret) => ret,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

unsafe fn _grow_the_stack(stack_size: usize, old_limit: usize, mut f: &mut FnMut()) {
    // Align to 16-bytes (see below for why)
    let stack_size = (stack_size + 15) / 16 * 16;

    // Allocate some new stack for oureslves
    let mut stack = Vec::<u8>::with_capacity(stack_size);
    let new_limit = stack.as_ptr() as usize + 32 * 1024;

    // Prepare stack limits for the stack switch
    set_stack_limit(new_limit);

    // Make sure the stack is 16-byte aligned which should be enough for all
    // platforms right now. Allocations on 64-bit are already 16-byte aligned
    // and our switching routine doesn't push any other data, but the routine on
    // 32-bit pushes an argument so we need a bit of an offset to get it 16-byte
    // aligned when the call is made.
    let offset = if cfg!(target_pointer_width = "32") {
        12
    } else {
        0
    };
    __stacker_switch_stacks(stack.as_mut_ptr() as usize + stack_size - offset,
                            doit as usize as *const _,
                            &mut f as *mut &mut FnMut() as *mut u8);

    // Once we've returned reset bothe stack limits and then return value same
    // value the closure returned.
    set_stack_limit(old_limit);

    unsafe extern fn doit(f: &mut &mut FnMut()) {
        f();
    }
}

cfg_if! {
    if #[cfg(windows)] {
        // See this for where all this logic is coming from.
        //
        // https://github.com/adobe/webkit/blob/0441266/Source/WTF/wtf
        //                   /StackBounds.cpp
        unsafe fn guess_os_stack_limit() -> Option<usize> {
            #[cfg(target_pointer_width = "32")]
            extern {
                #[link_name = "__stacker_get_tib_32"]
                fn get_tib_address() -> *const usize;
            }
            #[cfg(target_pointer_width = "64")]
            extern "system" {
                #[cfg_attr(target_env = "msvc", link_name = "NtCurrentTeb")]
                #[cfg_attr(target_env = "gnu", link_name = "__stacker_get_tib_64")]
                fn get_tib_address() -> *const usize;
            }
            // https://en.wikipedia.org/wiki/Win32_Thread_Information_Block for
            // the struct layout of the 32-bit TIB. It looks like the struct
            // layout of the 64-bit TIB is also the same for getting the stack
            // limit: http://doxygen.reactos.org/d3/db0/structNT__TIB64.html
            Some(*get_tib_address().offset(2))
        }
    } else if #[cfg(target_os = "linux")] {
        use std::mem;

        unsafe fn guess_os_stack_limit() -> Option<usize> {
            let mut attr: libc::pthread_attr_t = mem::zeroed();
            assert_eq!(libc::pthread_attr_init(&mut attr), 0);
            assert_eq!(libc::pthread_getattr_np(libc::pthread_self(),
                                                &mut attr), 0);
            let mut stackaddr = 0 as *mut _;
            let mut stacksize = 0;
            assert_eq!(libc::pthread_attr_getstack(&attr, &mut stackaddr,
                                                   &mut stacksize), 0);
            assert_eq!(libc::pthread_attr_destroy(&mut attr), 0);
            Some(stackaddr as usize)
        }
    } else if #[cfg(target_os = "macos")] {
        use libc::{c_void, pthread_t, size_t};

        unsafe fn guess_os_stack_limit() -> Option<usize> {
            Some(libc::pthread_get_stackaddr_np(libc::pthread_self()) as usize -
                libc::pthread_get_stacksize_np(libc::pthread_self()) as usize)
        }
    } else {
        unsafe fn guess_os_stack_limit() -> Option<usize> {
            None
        }
    }
}
