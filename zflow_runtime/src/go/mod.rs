mod go_std;
// mod utils;

use crate::component::{Component, ComponentOptions, GraphDefinition, ModuleComponent};
// use crate::go::utils::ZipReader;
use crate::ip::IPType;
use crate::port::{InPort, OutPort, PortOptions};
use crate::process::{ProcessError, ProcessOutput, ProcessResult};


use crate::goengine::ffi::*;

use goengine::SourceReader;
use is_url::is_url;
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::any::Any;
use std::borrow::{Borrow, BorrowMut};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fs::{self, File};
use std::future::Future;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::rc::Rc;

fn default_base_dir() -> String {
    "/".to_string()
}

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct GoComponent {
    pub name: String,
    pub inports: HashMap<String, PortOptions>,
    pub outports: HashMap<String, PortOptions>,
    #[serde(default)]
    /// Set the default component description
    pub description: String,
    #[serde(default)]
    /// Set the default component icon
    pub icon: String,
    #[serde(default)]
    /// Whether the component should keep send packets
    /// out in the order they were received
    pub ordered: bool,
    #[serde(default)]
    /// Whether the component should activate when it receives packets
    pub activate_on_input: bool,
    #[serde(default)]
    /// Bracket forwarding rules. By default we forward
    pub forward_brackets: HashMap<String, Vec<String>>,
    #[serde(default = "default_base_dir")]
    /// Base directory of lua sources
    pub base_dir: String,
    /// Path to lua source
    pub source: String,
    #[serde(default)]
    pub package_id: String,
    #[serde(default)]
    pub metadata: Map<String, Value>,
}

impl GoComponent {
    pub fn from_metadata(meta: Value) -> Option<GoComponent> {
        GoComponent::deserialize(meta).ok()
    }

    pub fn with_metadata(&mut self, meta: Value) -> GoComponent {
        if let Some(meta) = GoComponent::from_metadata(meta.clone()) {
            self.inports.extend(meta.inports);
            self.outports.extend(meta.outports);

            if !meta.description.is_empty() {
                self.description = meta.description;
            }

            if !meta.icon.is_empty() {
                self.icon = meta.icon;
            }
            self.forward_brackets.extend(meta.forward_brackets);
            if !meta.base_dir.is_empty() {
                self.base_dir = meta.base_dir;
            }
        } else if let Some(meta) = meta.clone().as_object() {
            self.metadata = meta.clone();
        }
        self.clone()
    }
}

impl GraphDefinition for GoComponent {
    fn to_any(&self) -> &dyn Any {
        Box::leak(Box::new(self.clone())) as &dyn Any
    }
}

impl ModuleComponent for GoComponent {
    fn as_component(&self) -> Result<Component, String> {
        let node_name = self.name.clone();
        let mut code = PathBuf::from(&self.base_dir);
        // let base_dir = self.base_dir.clone();

        let source = if is_url(&self.source) || self.base_dir != "/" {
            code.push(self.source.clone());
            fs::read_to_string(code).expect("Could not read lua code")
        } else {
            self.source.clone()
        };

        let mut inports = self.inports.clone();
        let mut outports = self.outports.clone();
        if inports.is_empty() {
            inports.insert("in".to_string(), PortOptions::default());
        }

        if outports.is_empty() {
            outports.insert("out".to_string(), PortOptions::default());
        }

        let go_std = go_std::get_go_std()
            .map_err(|err| format!("Could not load Go standard library: {:?}", err))?;

        return Ok(Component::new(ComponentOptions {
            metadata: Some(self.metadata.clone()),
            in_ports: HashMap::from_iter(
                inports
                    .clone()
                    .iter()
                    .map(|(key, options)| (key.clone(), InPort::new(options.clone())))
                    .collect::<Vec<_>>(),
            ),
            out_ports: HashMap::from_iter(
                outports
                    .clone()
                    .iter()
                    .map(|(key, options)| (key.clone(), OutPort::new(options.clone())))
                    .collect::<Vec<_>>(),
            ),
            description: self.description.clone(),
            icon: self.icon.clone(),
            ordered: self.ordered,
            activate_on_input: self.activate_on_input,
            forward_brackets: self.forward_brackets.clone(),
            // question: why doesnt a component have a graph?
            graph: None,
            process: Some(Box::new(move |handle| {
                let inputs: Vec<&String> = inports.keys().collect();
                let handle_binding = handle.clone();
                let mut handle_binding = handle_binding.try_lock();

                // todo: in order to make sense of the implementation, this variable is renamed to process_handler
                // to remove before final PR approval/merge/submission
                let process_handler = handle_binding.as_mut().map_err(|_| {
                    ProcessError(String::from("Process Handle dropped. Could not lock."))
                })?;

                let controlled = inports
                    .iter()
                    .filter(|(key, value)| value.control)
                    .map(|(key, value)| key)
                    .collect::<Vec<_>>();
                let controlled_data = controlled
                    .iter()
                    .map(|(key)| process_handler.input().get(key.clone()))
                    .collect::<Vec<_>>();

                // None can take a reference? interesting....
                if !controlled.is_empty() && controlled_data.contains(&None) {
                    return Ok(ProcessResult::default());
                }

                let _inputs: HashMap<&String, Value> = HashMap::from_iter(
                    inputs
                        .clone()
                        .iter()
                        .map(|port| {
                            let value = process_handler.input().get(*port);
                            if let Some(value) = value {
                                return (
                                    port.clone(),
                                    match value.datatype {
                                        IPType::Data(v) => v,
                                        _ => Value::Null,
                                    },
                                );
                            }
                            return (port.clone(), Value::Null);
                        })
                        .collect::<Vec<_>>(),
                );

                static mut process_output: OnceCell<ProcessOutput> = OnceCell::new();
                static mut process_input: OnceCell<Value> = OnceCell::new();

                unsafe {
                    process_input.set(json!(_inputs));
                    process_output.set(process_handler.output());
                }

                // HERE BE DRAGONS. insha allah and vibes.
                // this is the beginning of the go runtime implementation itself
                let mut cfg = goengine::Config::default();
                cfg.trace_parser = false;
                cfg.trace_checker = false;

                let src = source.clone();
                let (sr, path) = goengine::SourceReader::zip_lib_and_string(
                    std::borrow::Cow::Owned(go_std.clone()),
                    PathBuf::from("std/"),
                    std::borrow::Cow::Owned(src),
                );

                /// zflow golang struct/module
                #[derive(Ffi)]
                pub struct ZflowFfi {}

                #[ffi_impl(rename = "zflow.process")]
                impl ZflowFfi {
                    /// send function to send data out via the outport
                    fn ffi_send(args: GosValue) -> RuntimeResult<()> {
                        let data = go_value_to_json_value(&args).map_err(|err| err.0)?;
                        // get process handle
                        if let Some(process) = unsafe { process_output.get_mut() } {
                            process.send(&data).map_err(|err| err.0)?;
                        }
                        Ok(())
                    }
                    fn ffi_send_done(args: GosValue) -> RuntimeResult<()> {
                        let data = go_value_to_json_value(&args).map_err(|err| err.0)?;
                        // get process handle
                        if let Some(process) = unsafe { process_output.get_mut() } {
                            process.send_done(&data).map_err(|err| err.0)?;
                        }
                        Ok(())
                    }
                    fn ffi_inputs(ctx: &FfiCtx) -> RuntimeResult<GosValue> {
                        let v = unsafe {
                            process_input
                                .take()
                                .ok_or("Could not fetch process inputs".to_owned())?
                        };

                        let go_map = to_go_value(ctx, v);
                        Ok(go_map)
                    }
                }
                let mut engine = goengine::Engine::new();

                // Register the package: needs more research
                engine.register_extension("zflow.process", Rc::new(ZflowFfi {}));

                engine.set_std_io(cfg.std_in, cfg.std_out, cfg.std_err);
            
                let panic_handler: Option<Rc<dyn Fn(String, String)>> =
                    Some(Rc::new(move |msg: String, stack: String| {
                        eprintln!("{}\n", msg);
                        eprintln!("{}\n", stack);
                    }));
                engine
                    .run_source::<SourceReader>(
                        cfg.trace_parser,
                        cfg.trace_checker,
                        &sr,
                        &path,
                        panic_handler,
                    )
                    .map_err(|err| {
                        let errors = err
                            .borrow()
                            .iter()
                            .map(|v| v.msg.clone())
                            .collect::<Vec<_>>();
                        eprintln!("{:?}", errors);
                        ProcessError(format!("GoScript Rumtime error: {:?}", errors))
                    })?;

                // to silence the linter for now
                return Ok(ProcessResult::default());
            })),
        }));
    }
}

fn go_value_to_json_value(v: &GosValue) -> Result<Value, ProcessError> {
    // Todo: convert to serde_json::Value;
    if v.is_nil() {
        return Ok(Value::Null);
    }
    match v.typ() {
        ValueType::Void => Ok(Value::Null),
        ValueType::Bool => Ok(json!(*v.as_bool())),
        ValueType::Int => Ok(json!(*v.as_int())),
        ValueType::Int8 => Ok(json!(*v.as_int8())),
        ValueType::Int16 => Ok(json!(*v.as_int16())),
        ValueType::Int32 => Ok(json!(*v.as_int32())),
        ValueType::Int64 => Ok(json!(*v.as_int64())),
        ValueType::Uint => Ok(json!(*v.as_uint())),
        ValueType::UintPtr => Ok(json!(*v.as_uint_ptr())),
        ValueType::Uint8 => Ok(json!(*v.as_uint8())),
        ValueType::Uint16 => Ok(json!(*v.as_uint16())),
        ValueType::Uint32 => Ok(json!(*v.as_uint32())),
        ValueType::Uint64 => Ok(json!(*v.as_uint64())),
        ValueType::Float32 => Ok(json!(v.as_float32().0)),
        ValueType::Float64 => Ok(json!(v.as_float64().0)),
        ValueType::String => {
            let go_str = v.as_string().as_str();
            let string: &str = &go_str.as_ref();
            Ok(json!(string))
        }
        // is this feasible?
        ValueType::Array => {
            let (array_obj, d) = v.as_array::<GosElem>();

            let mut value = vec![];
            for data in array_obj.borrow_data().iter() {
                let d = data.clone().into_value(ValueType::Interface);
                if let Ok(underlying) = d.iface_underlying() {
                    if underlying.is_some() {
                        value.push(go_value_to_json_value(underlying.as_ref().unwrap())?);
                    }
                }
            }

            Ok(json!(value))
        }
        ValueType::Slice => {
            let mut value = vec![];
            if let Some(s) = v.clone().as_slice::<GosElem>() {
                let (slice_obj, d) = s.borrow();
                let (array_obj, d) = slice_obj.array().as_array::<GosElem>();
                for data in array_obj.borrow_data().iter() {
                    let d = data.clone().into_value(ValueType::Interface);
                    if let Ok(underlying) = d.iface_underlying() {
                        if underlying.is_some() {
                            value.push(go_value_to_json_value(underlying.as_ref().unwrap())?);
                        }
                    }
                }
            }
            Ok(json!(value))
        }
        ValueType::Map => {
            let mut m = Map::new();
            if let Some((mp, _)) = v.as_map() {
                let mp = mp.clone();
                for (k, v) in mp.borrow_data_mut().clone().into_iter() {
                    if k.typ() != ValueType::String {
                        return Err(ProcessError(format!(
                            "Hash Map should only be indexed by string"
                        )));
                    }
                    let key = go_value_to_json_value(&k)?.as_str().unwrap().to_string();
                    m.insert(key, go_value_to_json_value(&v)?);
                }
            }
            Ok(json!(m))
        }
        ValueType::Interface => {
            return go_value_to_json_value(v.as_interface().unwrap().underlying_value().unwrap())
        }
        _ => Err(ProcessError(format!("unsupported GoScript type"))),
    }
}

fn map_go_types(v: Value) -> ValueType {
    match v {
        Value::Null => ValueType::Void,
        Value::Bool(_) => ValueType::Bool,
        Value::Number(n) => {
            if n.is_f64() {
                ValueType::Float64
            } else if n.is_i64() {
                ValueType::Int64
            } else {
                ValueType::Uint64
            }
        }
        Value::String(_) => ValueType::String,
        Value::Array(_) => ValueType::Array,
        Value::Object(_) => ValueType::Map,
    }
}

fn to_go_value(ctx: &FfiCtx, value: Value) -> GosValue {
    match value.clone() {
        Value::Null => FfiCtx::new_nil(ValueType::Void),
        Value::Bool(b) => GosValue::from(b),
        Value::Number(d) => {
            if d.is_f64() {
                return GosValue::from(d.as_f64().unwrap());
            } else if d.is_u64() {
                return GosValue::from(d.as_u64().unwrap());
            }
            return GosValue::from(d.as_i64().unwrap());
        }
        Value::String(s) => FfiCtx::new_string(&s),
        Value::Array(a) => {
            let data = a
                .iter()
                .map(|d| to_go_value(ctx, d.clone()))
                .collect::<Vec<_>>();

            ctx.new_array(data, map_go_types(value))
        }
        Value::Object(m) => {
            let mp = MapObj::new();
            for (k, v) in m {
                let _val = FfiCtx::new_interface(to_go_value(ctx, v.clone()), None);
                mp.insert(FfiCtx::new_string(&k), _val);
            }

            ctx.new_map(mp)
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use zflow_graph::Graph;

    use crate::network::{BaseNetwork, Network, NetworkOptions};

    #[test]
    fn create_go_component() {
        let mut base_dir = std::env::current_dir().unwrap();
        base_dir.push("test_components");
        let base_dir = base_dir.to_str().unwrap();

        let mut graph = Graph::new("", false);
        graph
            .add_node("zflow", "add_go", None)
            .add_initial(json!(1), "zflow", "left", None)
            .add_initial(json!(2), "zflow", "right", None);

        let mut network = Network::create(
            graph.clone(),
            NetworkOptions {
                subscribe_graph: false,
                delay: true,
                base_dir: base_dir.to_string(),
                ..Default::default()
            },
        );

        if let Ok(nw) = network.connect().unwrap().try_lock().as_mut() {
            nw.start().unwrap();
        }
    }
}
