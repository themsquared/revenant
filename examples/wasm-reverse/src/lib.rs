//! Example revenant WASM plugin: `wasm_reverse` reverses a string, entirely
//! inside the sandbox. It imports nothing from the host — it is a pure
//! function of its JSON args — which is exactly why it is safe to load a
//! stranger's copy of it. See revenant-wasm for the host ABI.

use std::mem;

/// Reserve `len` bytes and hand the host the pointer to write args into.
#[no_mangle]
pub extern "C" fn revenant_alloc(len: i32) -> i32 {
    let mut buf: Vec<u8> = Vec::with_capacity(len as usize);
    let ptr = buf.as_mut_ptr();
    mem::forget(buf);
    ptr as i32
}

/// Return a JSON `ToolSpec`, packed as `(ptr << 32) | len`.
#[no_mangle]
pub extern "C" fn revenant_spec() -> i64 {
    let spec = br#"{"name":"wasm_reverse","description":"Reverse a string inside a sandboxed WASM guest.","permission":"read_only","input_schema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}}"#;
    leak_packed(spec.to_vec())
}

/// Read args JSON at `ptr/len`, reverse `text`, return a result JSON.
#[no_mangle]
pub extern "C" fn revenant_invoke(ptr: i32, len: i32) -> i64 {
    let args = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    let result = match serde_json::from_slice::<serde_json::Value>(args) {
        Ok(v) => {
            let text = v.get("text").and_then(|t| t.as_str()).unwrap_or("");
            let reversed: String = text.chars().rev().collect();
            serde_json::json!({ "ok": true, "content": reversed })
        }
        Err(e) => serde_json::json!({ "ok": false, "content": format!("bad args: {e}") }),
    };
    leak_packed(serde_json::to_vec(&result).unwrap())
}

/// Leak a byte buffer and return `(ptr << 32) | len` for the host to read.
fn leak_packed(bytes: Vec<u8>) -> i64 {
    let boxed = bytes.into_boxed_slice();
    let len = boxed.len();
    let ptr = boxed.as_ptr() as u64;
    mem::forget(boxed);
    ((ptr << 32) | len as u64) as i64
}
