//! Luau's require-by-string over a host navigator: relative paths, caching, proxy requires,
//! registered aliases, and cache control.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use l3i::Runtime;
use l3i::bind::Call;
use l3i::require::{Load, Navigate, RequireNavigator};
use l3i::stack::Scope;

/// Modules live at `/`-separated paths; the navigator position is a path stack.
struct Memory {
    modules: HashMap<String, String>,
    position: RefCell<Vec<String>>,
    loads: Rc<Cell<u32>>,
}

impl Memory {
    fn path(&self) -> String {
        self.position.borrow().join("/")
    }
}

impl RequireNavigator for Memory {
    fn reset(&self, requirer_chunkname: &str) -> Navigate {
        // Chunk names are `@path`; the position starts at the requirer itself.
        let path = requirer_chunkname.trim_start_matches('@');
        *self.position.borrow_mut() = path.split('/').filter(|s| !s.is_empty()).map(str::to_owned).collect();
        Navigate::Success
    }
    fn to_parent(&self) -> Navigate {
        if self.position.borrow_mut().pop().is_some() { Navigate::Success } else { Navigate::NotFound }
    }
    fn to_child(&self, name: &str) -> Navigate {
        self.position.borrow_mut().push(name.to_owned());
        Navigate::Success
    }
    fn is_module_present(&self) -> bool {
        self.modules.contains_key(&self.path())
    }
    fn chunkname(&self) -> Option<String> {
        Some(format!("@{}", self.path()))
    }
    fn loadname(&self) -> Option<String> {
        Some(self.path())
    }
    fn cache_key(&self) -> Option<String> {
        Some(self.path())
    }
    fn load(&self, call: &Call<'_>, _path: &str, chunkname: &str, loadname: &str) -> l3i::Result<Load> {
        // `path` is the string the script wrote; `loadname` is what `loadname()` resolved it to.
        self.loads.set(self.loads.get() + 1);
        let source =
            self.modules.get(loadname).ok_or_else(|| l3i::Error::runtime(format!("no module at {loadname}")))?;
        let bytecode = l3i::source::compile(source, &Default::default())?;
        let name = std::ffi::CString::new(chunkname).unwrap();
        let state = call.state();
        let top = call.stack().top();
        // SAFETY: the chunk is loaded and run on the requiring thread; its results stay on the
        // call for Luau to collect. A load error leaves its message on top and is re-raised.
        unsafe {
            if l3i::ffi::luau_load(state, name.as_ptr(), bytecode.as_ptr().cast(), bytecode.len(), 0)
                != l3i::ffi::LUA_OK
            {
                return Err(l3i::Error::LuaErrorOnStack);
            }
            l3i::ffi::lua_call(state, 0, l3i::ffi::LUA_MULTRET);
            Ok(Load::Results(l3i::ffi::lua_gettop(state) - top))
        }
    }
}

fn navigator(loads: Rc<Cell<u32>>) -> Memory {
    let mut modules = HashMap::new();
    modules.insert("lib/math".to_owned(), "return { twice = function(x) return x * 2 end }".to_owned());
    modules.insert("lib/greet".to_owned(), "local math = require('./math') return { four = math.twice(2) }".to_owned());
    modules.insert("app/main".to_owned(), "local greet = require('../lib/greet') return greet.four".to_owned());
    Memory { modules, position: RefCell::new(Vec::new()), loads }
}

#[test]
fn require_resolves_relative_paths_through_the_navigator_and_caches_results() {
    let loads = Rc::new(Cell::new(0));
    let runtime = Runtime::new().unwrap();
    runtime.install_require(navigator(loads.clone())).unwrap();
    let result: i32 = runtime
        .stack()
        .with_frame(|frame| {
            let chunk = runtime.load(
                frame,
                "@app/main",
                "local greet = require('../lib/greet') return greet.four + require('../lib/math').twice(5)",
                &Default::default(),
            )?;
            chunk.as_function()?.invoke::<i32, ()>(frame, ())
        })
        .unwrap();
    assert_eq!(result, 14);
    assert_eq!(loads.get(), 2, "greet and math each loaded once; math was cached for the second require");
    // Proxy require resolves relative to a named module.
    let proxy = runtime.proxy_require_function().unwrap();
    let four: i32 = proxy
        .invoke_with(&runtime.stack(), ("./greet", "@lib/x"), |frame, view| {
            view.as_table()?.get_as::<i32>(frame, "four")
        })
        .unwrap();
    assert_eq!(four, 4);
    // Registered alias results bypass navigation entirely.
    let table = l3i::value::Table::new(&runtime.stack(), 0, 1).unwrap();
    table.set(&runtime.stack(), "answer", &42i32).unwrap();
    runtime.register_require_module("@answers", table.value()).unwrap();
    runtime.exec("assert(require('@answers').answer == 42)").unwrap();
    // Clearing the cache forces reloads.
    runtime.clear_require_cache_entry("lib/math").unwrap();
    runtime
        .stack()
        .with_frame(|frame| {
            let chunk =
                runtime.load(frame, "@app/other", "return require('../lib/math').twice(1)", &Default::default())?;
            chunk.as_function()?.invoke::<i32, ()>(frame, ())
        })
        .unwrap();
    assert_eq!(loads.get(), 3);
    runtime.clear_require_cache().unwrap();
    runtime
        .stack()
        .with_frame(|frame| {
            let chunk =
                runtime.load(frame, "@app/other", "return require('../lib/greet').four", &Default::default())?;
            chunk.as_function()?.invoke::<i32, ()>(frame, ())
        })
        .unwrap();
    assert_eq!(loads.get(), 5, "both modules reloaded after a full clear");
    // Missing modules are script errors, not panics.
    let error = runtime
        .stack()
        .with_frame(|frame| {
            let chunk = runtime.load(frame, "@app/main", "return require('./nothing')", &Default::default())?;
            chunk.as_function()?.invoke::<i32, ()>(frame, ())
        })
        .unwrap_err()
        .to_string();
    assert!(error.contains("no module present"), "{error}");
}
