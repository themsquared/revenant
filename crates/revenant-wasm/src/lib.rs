//! revenant-wasm: sandboxed, dynamic plugins as WebAssembly tools.
//!
//! A `.wasm` file dropped in `~/.revenant/plugins/` becomes a tool the agent
//! can call — no recompile, no restart of the world, and no ambient authority.
//! The guest runs in a `wasmi` interpreter (pure Rust, no JIT — Pi-friendly)
//! with **fuel metering** (bounded CPU) and **no host imports** (v1): a WASM
//! tool is a pure function of its JSON args. It cannot open a file, a socket,
//! or a subprocess, because the host grants it nothing to do so with. That is
//! the ward: capability is something the host *gives*, never something the
//! guest *takes*.
//!
//! # ABI (core wasm, host is little-endian)
//! The guest exports linear `memory` plus:
//! - `revenant_alloc(len: i32) -> i32` — reserve `len` bytes, return the ptr.
//! - `revenant_spec() -> i64` — return a `ToolSpec` JSON, packed `(ptr<<32)|len`.
//! - `revenant_invoke(ptr: i32, len: i32) -> i64` — read args JSON at `ptr/len`,
//!   return a result JSON `{"ok":bool,"content":string}`, packed `(ptr<<32)|len`.

use anyhow::{anyhow, bail, Context, Result};
use revenant_core::{PermissionTier, ToolOutput, ToolSpec};
use revenant_tools::{Tool, ToolCx};
use std::path::Path;
use std::sync::Arc;
use wasmi::{Engine, Linker, Memory, Module, Store};

/// CPU ceiling per call, in wasmi fuel units. Generous for real compute, but
/// finite — a guest that spins forever runs out of fuel and the call errors
/// instead of wedging a turn.
const DEFAULT_FUEL: u64 = 200_000_000;

/// A dynamically-loaded WASM tool. Holds the compiled module + engine (both
/// cheap to clone and `Send + Sync`); every invocation gets a fresh `Store`
/// and `Instance`, so guest state never leaks between calls.
pub struct WasmTool {
    engine: Engine,
    module: Module,
    spec: ToolSpec,
    permission: PermissionTier,
    fuel: u64,
}

impl WasmTool {
    /// Compile a guest, read its self-declared spec, and wrap it as a `Tool`.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let mut config = wasmi::Config::default();
        config.consume_fuel(true);
        let engine = Engine::new(&config);
        let module = Module::new(&engine, bytes).context("compiling wasm module")?;

        // Read the spec once at load. A guest that can't describe itself is
        // not loadable.
        let spec_bytes = call_json(&engine, &module, DEFAULT_FUEL, Export::Spec)?;
        let raw: RawSpec =
            serde_json::from_slice(&spec_bytes).context("parsing tool spec JSON from guest")?;
        if raw.name.trim().is_empty() {
            bail!("guest tool spec has an empty name");
        }
        let permission = raw.permission.as_deref().map(parse_permission).unwrap_or(
            // Pure-compute guests default to the least authority. (They have
            // no host imports anyway; this is belt-and-suspenders.)
            PermissionTier::ReadOnly,
        );
        let spec = ToolSpec {
            name: raw.name,
            description: raw.description,
            input_schema: raw
                .input_schema
                .unwrap_or_else(|| serde_json::json!({"type":"object"})),
        };
        Ok(WasmTool { engine, module, spec, permission, fuel: DEFAULT_FUEL })
    }
}

#[async_trait::async_trait]
impl Tool for WasmTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }
    fn permission(&self) -> PermissionTier {
        self.permission
    }
    async fn invoke(&self, _cx: &ToolCx, args: serde_json::Value) -> ToolOutput {
        let engine = self.engine.clone();
        let module = self.module.clone();
        let fuel = self.fuel;
        let args_bytes = match serde_json::to_vec(&args) {
            Ok(b) => b,
            Err(e) => return ToolOutput::err(format!("serializing args: {e}")),
        };
        // wasmi is synchronous; run it off the async executor so a heavy guest
        // never blocks the runtime.
        let out = tokio::task::spawn_blocking(move || {
            call_json(&engine, &module, fuel, Export::Invoke(&args_bytes))
        })
        .await;
        match out {
            Ok(Ok(bytes)) => match serde_json::from_slice::<RawResult>(&bytes) {
                Ok(r) if r.ok => ToolOutput::ok(r.content),
                Ok(r) => ToolOutput::err(r.content),
                Err(e) => ToolOutput::err(format!("guest returned invalid result JSON: {e}")),
            },
            Ok(Err(e)) => ToolOutput::err(format!("wasm tool failed: {e:#}")),
            Err(e) => ToolOutput::err(format!("wasm task panicked: {e}")),
        }
    }
}

/// Load every `*.wasm` in `dir` as a tool. Missing dir → empty (not an error).
/// A single bad module is logged and skipped so one broken plugin can't sink
/// the rest of the horde.
pub fn load_dir(dir: &Path) -> Vec<Arc<dyn Tool>> {
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return tools,
    };
    let mut paths: Vec<_> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "wasm"))
        .collect();
    paths.sort();
    for path in paths {
        match std::fs::read(&path).map_err(anyhow::Error::from).and_then(|b| WasmTool::from_bytes(&b))
        {
            Ok(tool) => {
                tracing::info!("loaded wasm tool '{}' from {}", tool.spec().name, path.display());
                tools.push(Arc::new(tool));
            }
            Err(e) => tracing::warn!("skipping wasm plugin {}: {e:#}", path.display()),
        }
    }
    tools
}

// --- guest ABI plumbing --------------------------------------------------

enum Export<'a> {
    Spec,
    Invoke(&'a [u8]),
}

#[derive(serde::Deserialize)]
struct RawSpec {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    input_schema: Option<serde_json::Value>,
    #[serde(default)]
    permission: Option<String>,
}

#[derive(serde::Deserialize)]
struct RawResult {
    ok: bool,
    #[serde(default)]
    content: String,
}

fn parse_permission(s: &str) -> PermissionTier {
    match s.to_ascii_lowercase().replace(['-', ' '], "_").as_str() {
        "write_workspace" => PermissionTier::WriteWorkspace,
        "network" => PermissionTier::Network,
        "dangerous" => PermissionTier::Dangerous,
        _ => PermissionTier::ReadOnly,
    }
}

fn unpack(v: i64) -> (u32, u32) {
    let u = v as u64;
    ((u >> 32) as u32, (u & 0xffff_ffff) as u32)
}

fn read_mem(mem: &Memory, store: &Store<()>, ptr: u32, len: u32) -> Result<Vec<u8>> {
    let data = mem.data(store);
    let (start, end) = (ptr as usize, ptr as usize + len as usize);
    data.get(start..end)
        .map(|s| s.to_vec())
        .ok_or_else(|| anyhow!("guest returned out-of-bounds ptr/len ({ptr}, {len})"))
}

/// Instantiate a fresh store, call one export, and return the JSON bytes it
/// points at. The instance is discarded on return — no state survives.
fn call_json(engine: &Engine, module: &Module, fuel: u64, export: Export) -> Result<Vec<u8>> {
    let mut store = Store::new(engine, ());
    store.set_fuel(fuel).context("setting fuel")?;
    // Empty linker: the guest imports nothing, so it can do nothing but compute.
    let linker = <Linker<()>>::new(engine);
    let instance = linker
        .instantiate(&mut store, module)
        .context("instantiating guest")?
        .start(&mut store)
        .context("running guest start")?;
    let memory = instance
        .get_memory(&store, "memory")
        .ok_or_else(|| anyhow!("guest does not export 'memory'"))?;

    let packed = match export {
        Export::Spec => {
            let f = instance
                .get_typed_func::<(), i64>(&store, "revenant_spec")
                .context("guest missing revenant_spec() -> i64")?;
            f.call(&mut store, ()).context("calling revenant_spec")?
        }
        Export::Invoke(args) => {
            let alloc = instance
                .get_typed_func::<i32, i32>(&store, "revenant_alloc")
                .context("guest missing revenant_alloc(i32) -> i32")?;
            let ptr = alloc
                .call(&mut store, args.len() as i32)
                .context("calling revenant_alloc")?;
            memory
                .write(&mut store, ptr as usize, args)
                .context("writing args into guest memory")?;
            let f = instance
                .get_typed_func::<(i32, i32), i64>(&store, "revenant_invoke")
                .context("guest missing revenant_invoke(i32,i32) -> i64")?;
            f.call(&mut store, (ptr, args.len() as i32)).context("calling revenant_invoke")?
        }
    };
    let (ptr, len) = unpack(packed);
    read_mem(&memory, &store, ptr, len)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A minimal guest in WebAssembly text: a static byte blob holds the spec
    // JSON at offset 0 and a canned result JSON after it; the exported
    // functions just return packed pointers into that blob. It exercises the
    // full host ABI (spec + alloc + invoke + memory read) with no toolchain.
    fn fixture_wasm() -> Vec<u8> {
        let spec = br#"{"name":"echo_wat","description":"wat fixture","permission":"read_only"}"#;
        let result = br#"{"ok":true,"content":"pong"}"#;
        let spec_len = spec.len();
        let result_off = spec_len;
        let result_len = result.len();
        let mut data = Vec::new();
        data.extend_from_slice(spec);
        data.extend_from_slice(result);
        // Escape the data blob for WAT.
        let escaped: String = data.iter().map(|b| format!("\\{b:02x}")).collect();
        let wat = format!(
            r#"(module
  (memory (export "memory") 1)
  (data (i32.const 0) "{escaped}")
  (func (export "revenant_alloc") (param i32) (result i32)
    (i32.const 1024))
  (func (export "revenant_spec") (result i64)
    (i64.or (i64.shl (i64.const 0) (i64.const 32)) (i64.const {spec_len})))
  (func (export "revenant_invoke") (param i32 i32) (result i64)
    (i64.or (i64.shl (i64.const {result_off}) (i64.const 32)) (i64.const {result_len})))
)"#
        );
        wat::parse_str(wat).expect("assemble fixture wasm")
    }

    #[test]
    fn loads_spec_and_permission() {
        let tool = WasmTool::from_bytes(&fixture_wasm()).unwrap();
        assert_eq!(tool.spec().name, "echo_wat");
        assert_eq!(tool.permission(), PermissionTier::ReadOnly);
    }

    #[test]
    fn invoke_round_trips_through_guest_memory() {
        // Exercise the full host ABI (alloc + write args + invoke + memory
        // read + result parse) without constructing a runtime ToolCx.
        let tool = WasmTool::from_bytes(&fixture_wasm()).unwrap();
        let args = br#"{"x":1}"#;
        let bytes =
            call_json(&tool.engine, &tool.module, tool.fuel, Export::Invoke(args)).unwrap();
        let r: RawResult = serde_json::from_slice(&bytes).unwrap();
        assert!(r.ok);
        assert_eq!(r.content, "pong");
    }

    #[test]
    fn fuel_is_bounded() {
        // A guest that never returns must exhaust fuel and error, not hang.
        let wat = r#"(module
  (memory (export "memory") 1)
  (func (export "revenant_spec") (result i64)
    (loop (br 0)) (i64.const 0)))"#;
        let bytes = wat::parse_str(wat).unwrap();
        assert!(WasmTool::from_bytes(&bytes).is_err());
    }

    #[test]
    fn bad_module_is_an_error_not_a_panic() {
        assert!(WasmTool::from_bytes(b"not wasm").is_err());
    }
}
