//! Instance-local storage.
//!
//! At runtime, an instance of a Node.js addon can contain its own local storage,
//! which can then be shared and accessed as needed between Rust modules. This can
//! be useful for setting up long-lived state that needs to be shared between calls
//! of an addon's APIs.
//!
//! For example, an addon may wish to track the [thread ID][threadId] of each of its
//! instances:
//!
//! ```
//! # use neon::prelude::*;
//! # use neon::instance::Local;
//! static THREAD_ID: Local<u32> = Local::new();
//!
//! pub fn thread_id<'cx, C: Context<'cx>>(cx: &mut C) -> NeonResult<u32> {
//!     THREAD_ID.get_or_try_init(cx, |cx| {
//!         let global = cx.global();
//!         let require: Handle<JsFunction> = global.get(cx, "require")?;
//!         let worker: Handle<JsObject> = require.call_with(cx)
//!             .arg(cx.string("node:worker_threads"))
//!             .apply(cx)?;
//!         let threadId: Handle<JsNumber> = worker.get(cx, "threadId")?;
//!         Ok(threadId.value(cx) as u32)
//!     }).cloned()
//! }
//! ```
//!
//! ### The Addon Lifecycle
//!
//! For some use cases, a single shared global constant stored in a `static` variable
//! might be sufficient:
//!
//! ```
//! static MY_CONSTANT: &'static str = "hello Neon";
//! ```
//!
//! This variable will be allocated when the addon is first loaded into the Node.js
//! process. This works fine for single-threaded applications, or global immutable
//! data.
//!
//! However, since the addition of [worker threads][workers] in Node v10,
//! modules can be instantiated multiple times in a single Node process. This means
//! that while the dynamically-loaded binary library (i.e., the Rust implementation of
//! the addon) is only loaded once in the running process, but its `main()` function
//! is executed multiple times with distinct module objects, once per application thread:
//!
//! ![The Node.js addon lifecycle, described in detail below.][lifecycle]
//!
//! This means that any instance-local data needs to be initialized separately for each
//! instance of the addon. This module provides a simple container type, [`Local`](Local),
//! for allocating and initializing instance-local data. For example, a custom datatype
//! cannot be shared across separate threads and must be instance-local:
//!
//! ```
//! # use neon::prelude::*;
//! # use neon::instance::Local;
//! # fn initialize_my_datatype<'cx, C: Context<'cx>>(cx: &mut C) -> JsResult<'cx, JsFunction> { unimplemented!() }
//! static MY_CONSTRUCTOR: Local<Root<JsFunction>> = Local::new();
//!
//! pub fn my_constructor<'cx, C: Context<'cx>>(cx: &mut C) -> JsResult<'cx, JsFunction> {
//!     let constructor = MY_CONSTRUCTOR.get_or_try_init(cx, |cx| {
//!         let constructor: Handle<JsFunction> = initialize_my_datatype(cx)?;
//!         Ok(constructor.root(cx))
//!     })?;
//!     Ok(constructor.to_inner(cx))
//! }
//! ```
//!
//! ### When to Use Instance-Local Storage
//!
//! Single-threaded applications don't generally need to worry about instance data.
//! There are two cases where Neon apps should consider storing static data in a
//! `Local` storage cell:
//!
//! - **Multi-threaded applications:** If your Node application uses the `Worker`
//!   API, you'll want to store any static data that might get access from multiple
//!   threads in instance-local data.
//! - **Libraries:** If your addon is part of a library that could be used by multiple
//!   applications, you'll want to store static data in instance-local data in case the
//!   addon ends up instantiated by multiple threads in some future application.
//!
//! [lifecycle]: https://raw.githubusercontent.com/neon-bindings/neon/main/doc/lifecycle.png
//! [workers]: https://nodejs.org/api/worker_threads.html
//! [threadId]: https://nodejs.org/api/worker_threads.html#workerthreadid

use std::any::Any;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicUsize, Ordering};

use once_cell::sync::OnceCell;

use crate::context::Context;
use crate::lifecycle::LocalCell;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn next_id() -> usize {
    COUNTER.fetch_add(1, Ordering::SeqCst)
}

/// A cell that can be used to allocate data that is local to an instance
/// of a Neon addon.
#[derive(Default)]
pub struct Local<T> {
    _type: PhantomData<T>,
    id: OnceCell<usize>,
}

impl<T> Local<T> {
    /// Creates a new local value. This method is `const`, so it can be assigned to
    /// static variables.
    pub const fn new() -> Self {
        Self {
            _type: PhantomData,
            id: OnceCell::new(),
        }
    }

    fn id(&self) -> usize {
        *self.id.get_or_init(next_id)
    }
}

impl<T: Any + Send + 'static> Local<T> {
    /// Gets the current value of the cell. Returns `None` if the cell has not
    /// yet been initialized.
    pub fn get<'cx, 'a, C>(&self, cx: &'a mut C) -> Option<&'cx T>
    where
        C: Context<'cx>,
    {
        // Unwrap safety: The type bound Local<T> and the fact that every Local has a unique
        // id guarantees that the cell is only ever assigned instances of type T.
        let r: Option<&T> =
            LocalCell::get(cx, self.id()).map(|value| value.downcast_ref().unwrap());

        // Safety: Since the Box is immutable and heap-allocated, it's guaranteed not to
        // move or change for the duration of the context.
        unsafe { std::mem::transmute::<Option<&'a T>, Option<&'cx T>>(r) }
    }

    /// Gets the current value of the cell, initializing it with `value` if it has
    /// not yet been initialized.
    pub fn get_or_init<'cx, 'a, C>(&self, cx: &'a mut C, value: T) -> &'cx T
    where
        C: Context<'cx>,
    {
        // Unwrap safety: The type bound Local<T> and the fact that every Local has a unique
        // id guarantees that the cell is only ever assigned instances of type T.
        let r: &T = LocalCell::get_or_init(cx, self.id(), Box::new(value))
            .downcast_ref()
            .unwrap();

        // Safety: Since the Box is immutable and heap-allocated, it's guaranteed not to
        // move or change for the duration of the context.
        unsafe { std::mem::transmute::<&'a T, &'cx T>(r) }
    }

    /// Gets the current value of the cell, initializing it with the result of
    /// calling `f` if it has not yet been initialized.
    pub fn get_or_init_with<'cx, 'a, C, F>(&self, cx: &'a mut C, f: F) -> &'cx T
    where
        C: Context<'cx>,
        F: FnOnce() -> T,
    {
        // Unwrap safety: The type bound Local<T> and the fact that every Local has a unique
        // id guarantees that the cell is only ever assigned instances of type T.
        let r: &T = LocalCell::get_or_init_with(cx, self.id(), || Box::new(f()))
            .downcast_ref()
            .unwrap();

        // Safety: Since the Box is immutable and heap-allocated, it's guaranteed not to
        // move or change for the duration of the context.
        unsafe { std::mem::transmute::<&'a T, &'cx T>(r) }
    }

    /// Gets the current value of the cell, initializing it with the result of
    /// calling `f` if it has not yet been initialized. Returns `Err` if the
    /// callback triggers a JavaScript exception.
    ///
    /// During the execution of `f`, calling any methods on this `Local` that
    /// attempt to initialize it will panic.
    pub fn get_or_try_init<'cx, 'a, C, E, F>(&self, cx: &'a mut C, f: F) -> Result<&'cx T, E>
    where
        C: Context<'cx>,
        F: FnOnce(&mut C) -> Result<T, E>,
    {
        // Unwrap safety: The type bound Local<T> and the fact that every Local has a unique
        // id guarantees that the cell is only ever assigned instances of type T.
        let r: &T = LocalCell::get_or_try_init(cx, self.id(), |cx| Ok(Box::new(f(cx)?)))?
            .downcast_ref()
            .unwrap();

        // Safety: Since the Box is immutable and heap-allocated, it's guaranteed not to
        // move or change for the duration of the context.
        Ok(unsafe { std::mem::transmute::<&'a T, &'cx T>(r) })
    }
}

impl<T: Any + Send + Default + 'static> Local<T> {
    /// Gets the current value of the cell, initializing it with the default value
    /// if it has not yet been initialized.
    pub fn get_or_init_default<'cx, 'a, C>(&self, cx: &'a mut C) -> &'cx T
    where
        C: Context<'cx>,
    {
        self.get_or_init_with(cx, Default::default)
    }
}
