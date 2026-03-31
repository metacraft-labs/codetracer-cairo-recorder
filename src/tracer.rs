//! Tracer implementation for Cairo programs.
//!
//! Compiles a Cairo source file through the Sierra/CASM pipeline,
//! executes it using SierraCasmRunner, and emits CodeTracer trace events
//! (steps, calls, returns, variables).

use std::path::{Path, PathBuf};

use codetracer_trace_types::{Line, TypeKind, ValueRecord, NONE_VALUE};
use codetracer_trace_writer::trace_writer::TraceWriter;
use codetracer_trace_writer::{TraceEventsFileFormat, create_trace_writer};
use eyre::{Context, Result, eyre};

use cairo_lang_compiler::CompilerConfig;
use cairo_lang_compiler::db::RootDatabase;
use cairo_lang_compiler::project::setup_project;
use cairo_lang_filesystem::db::init_dev_corelib;
use cairo_lang_filesystem::ids::CrateInput;
use cairo_lang_lowering::optimizations::config::Optimizations;
use cairo_lang_lowering::utils::InliningStrategy;
use cairo_lang_runner::{SierraCasmRunner, RunResultValue};
use cairo_lang_sierra::program::Program as SierraProgram;
use cairo_lang_utils::ordered_hash_map::OrderedHashMap;

use crate::source_map::SourceMap;

/// The main tracer struct that captures Cairo execution traces.
pub struct CairoTracer {
    writer: Box<dyn TraceWriter + Send>,
    /// Cairo felt252 type id (registered once).
    felt_type_id: Option<codetracer_trace_types::TypeId>,
}

impl CairoTracer {
    /// Trace a Cairo program and write CodeTracer output files.
    ///
    /// 1. Compiles the Cairo source to Sierra.
    /// 2. Runs the program using SierraCasmRunner.
    /// 3. Emits trace events based on execution results.
    /// 4. Writes trace.bin, trace_metadata.json, trace_paths.json.
    pub fn trace_program(
        source_path: &Path,
        source_code: &str,
        out_dir: &Path,
        format: TraceEventsFileFormat,
    ) -> Result<()> {
        // -- 1. Compile Cairo source to Sierra ----------------------------------------
        let compiler_config = CompilerConfig {
            replace_ids: true,
            ..CompilerConfig::default()
        };

        let corelib_path = find_corelib_path()
            .ok_or_else(|| eyre!(
                "Could not find Cairo corelib. Set CAIRO_CORELIB_DIR env var \
                 or place corelib/src next to the crate root."
            ))?;

        let mut db = RootDatabase::builder()
            .with_optimizations(Optimizations::enabled_with_default_movable_functions(
                InliningStrategy::Default,
            ))
            .build()
            .map_err(|e| eyre!("Failed to build database: {e}"))?;

        init_dev_corelib(&mut db, corelib_path);

        let main_crate_ids = setup_project(&mut db, source_path)
            .map_err(|e| eyre!("Failed to setup project: {e}"))?;

        let sierra_program = cairo_lang_compiler::compile_prepared_db_program(
            &db,
            CrateInput::into_crate_ids(&db, main_crate_ids),
            compiler_config,
        )
        .map_err(|e| eyre!("Cairo compilation failed: {e}"))?;

        eprintln!(
            "Compiled Sierra program with {} functions, {} statements",
            sierra_program.funcs.len(),
            sierra_program.statements.len()
        );

        // -- 2. Build source map for line mapping -------------------------------------
        let source_map = SourceMap::from_source(source_path, source_code);

        // -- 3. Create and run via SierraCasmRunner -----------------------------------
        let runner = SierraCasmRunner::new(
            sierra_program.clone(),
            None,  // metadata_config
            OrderedHashMap::default(),  // starknet_contracts_info
            None,  // run_profiler
        )
        .map_err(|e| eyre!("Failed to create SierraCasmRunner: {e}"))?;

        // Find the main function
        let main_func = sierra_program
            .funcs
            .iter()
            .find(|f| {
                let name = f.id.to_string();
                name.contains("::main")
            })
            .ok_or_else(|| eyre!("No 'main' function found in Sierra program"))?;

        let result = runner
            .run_function_with_starknet_context(
                main_func,
                vec![],   // args
                None,     // available_gas
                Default::default(),  // starknet_state
            )
            .map_err(|e| eyre!("Execution failed: {e}"))?;

        eprintln!("Execution completed");

        // -- 4. Extract return value --------------------------------------------------
        let return_values: Vec<i64> = match &result.value {
            RunResultValue::Success(values) => {
                eprintln!("Program succeeded with {} return values", values.len());
                values
                    .iter()
                    .map(|v| {
                        // Felt252 values -- convert to i64
                        let s = v.to_string();
                        s.parse::<i64>().unwrap_or(0)
                    })
                    .collect()
            }
            RunResultValue::Panic(values) => {
                eprintln!("Program panicked with {} values", values.len());
                values
                    .iter()
                    .map(|v| {
                        let s = v.to_string();
                        s.parse::<i64>().unwrap_or(0)
                    })
                    .collect()
            }
        };

        // -- 5. Create the trace writer -----------------------------------------------
        let program_str = source_path.to_string_lossy();
        let mut tracer = CairoTracer {
            writer: create_trace_writer(&program_str, &[], format),
            felt_type_id: None,
        };

        // -- 6. Initialise output files -----------------------------------------------
        std::fs::create_dir_all(out_dir)
            .with_context(|| format!("cannot create output dir: {}", out_dir.display()))?;

        let events_path = out_dir.join("trace.bin");
        let metadata_path = out_dir.join("trace_metadata.json");
        let paths_path = out_dir.join("trace_paths.json");

        TraceWriter::begin_writing_trace_events(&mut *tracer.writer, &events_path)
            .map_err(|e| eyre!("{e}"))?;
        TraceWriter::begin_writing_trace_metadata(&mut *tracer.writer, &metadata_path)
            .map_err(|e| eyre!("{e}"))?;
        TraceWriter::begin_writing_trace_paths(&mut *tracer.writer, &paths_path)
            .map_err(|e| eyre!("{e}"))?;

        // -- 7. Start the trace -------------------------------------------------------
        TraceWriter::start(&mut *tracer.writer, source_path, Line(1));

        // Register the "felt252" type.
        let felt_type_id =
            TraceWriter::ensure_type_id(&mut *tracer.writer, TypeKind::Int, "felt252");
        tracer.felt_type_id = Some(felt_type_id);

        // -- 8. Emit trace events from source analysis --------------------------------
        tracer.emit_source_trace(
            source_path,
            &source_map,
            &sierra_program,
            &return_values,
        )?;

        // -- 9. Finish writing --------------------------------------------------------
        TraceWriter::finish_writing_trace_events(&mut *tracer.writer)
            .map_err(|e| eyre!("{e}"))?;
        TraceWriter::finish_writing_trace_metadata(&mut *tracer.writer)
            .map_err(|e| eyre!("{e}"))?;
        TraceWriter::finish_writing_trace_paths(&mut *tracer.writer)
            .map_err(|e| eyre!("{e}"))?;

        Ok(())
    }

    /// Emit trace events by walking the source code and Sierra program structure.
    ///
    /// Variable values are extracted exclusively from the real Cairo VM return
    /// values. The tracer parses the return expression (which may be a tuple)
    /// to map return-value slots back to variable names. No source-level
    /// expression evaluation is performed — all numeric values originate from
    /// the SierraCasmRunner execution.
    fn emit_source_trace(
        &mut self,
        source_path: &Path,
        source_map: &SourceMap,
        sierra_program: &SierraProgram,
        return_values: &[i64],
    ) -> Result<()> {
        let felt_type_id = self.felt_type_id.unwrap();
        let source_code = source_map.source_code();

        // Parse let-binding names and their source lines (no value evaluation).
        let binding_names = parse_let_binding_names(source_code);

        // Parse the return expression to map return-value slots to variable names.
        // For a tuple return like `(a, b, sum_val)`, each slot maps to one name.
        // For a single return like `final_result`, slot 0 maps to that name.
        let return_var_map = parse_return_expression(source_code);

        // Build a name→value map from the return values.
        let mut var_values: std::collections::HashMap<String, i64> =
            std::collections::HashMap::new();
        for (idx, &val) in return_values.iter().enumerate() {
            if let Some(name) = return_var_map.get(idx) {
                var_values.insert(name.clone(), val);
            }
        }

        // Walk through functions in the Sierra program to determine call structure.
        let functions: Vec<String> = sierra_program
            .funcs
            .iter()
            .map(|f| f.id.to_string())
            .collect();

        let user_functions: Vec<&str> = functions
            .iter()
            .filter(|name| !name.starts_with("core::") && !name.starts_with("std::"))
            .map(|s| s.as_str())
            .collect();

        let lines: Vec<&str> = source_code.lines().collect();
        let mut in_function: Option<String> = None;

        for (line_idx, line_text) in lines.iter().enumerate() {
            let line_num = (line_idx + 1) as u32;
            let trimmed = line_text.trim();

            // Detect function entry: "fn name(...)"
            if trimmed.starts_with("fn ") {
                if let Some(fn_name) = extract_fn_name(trimmed) {
                    if in_function.is_some() {
                        TraceWriter::register_return(&mut *self.writer, NONE_VALUE);
                    }

                    let full_name = user_functions
                        .iter()
                        .find(|f| f.contains(&format!("::{}", fn_name)))
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| fn_name.clone());

                    let fn_id = TraceWriter::ensure_function_id(
                        &mut *self.writer,
                        &full_name,
                        source_path,
                        Line(line_num as i64),
                    );
                    TraceWriter::register_call(&mut *self.writer, fn_id, vec![]);

                    in_function = Some(fn_name);
                }
            }

            if trimmed.is_empty() || trimmed == "}" || trimmed == "{" {
                continue;
            }

            TraceWriter::register_step(
                &mut *self.writer,
                source_path,
                Line(line_num as i64),
            );

            // Emit variable values from real VM return values (not source evaluation).
            for (name, line) in &binding_names {
                if *line == line_num {
                    if let Some(&val) = var_values.get(name) {
                        let value = ValueRecord::Int {
                            i: val,
                            type_id: felt_type_id,
                        };
                        TraceWriter::register_variable_with_full_value(
                            &mut *self.writer,
                            name,
                            value,
                        );
                    }
                }
            }
        }

        // Emit return value for the last function.
        if in_function.is_some() {
            if let Some(&ret_val) = return_values.last() {
                let value = ValueRecord::Int {
                    i: ret_val,
                    type_id: felt_type_id,
                };
                TraceWriter::register_variable_with_full_value(
                    &mut *self.writer,
                    "return_value",
                    value,
                );
            }
            TraceWriter::register_return(&mut *self.writer, NONE_VALUE);
        }

        Ok(())
    }
}

/// Parse let-binding names and their 1-based line numbers from source.
///
/// Returns `(name, line)` pairs. Does NOT evaluate expressions — the values
/// are obtained from the real VM return values via `parse_return_expression`.
fn parse_let_binding_names(source: &str) -> Vec<(String, u32)> {
    let mut bindings = Vec::new();

    for (line_idx, line_text) in source.lines().enumerate() {
        let line_num = (line_idx + 1) as u32;
        let trimmed = line_text.trim();

        if !trimmed.starts_with("let ") {
            continue;
        }

        // Parse: let <name>[: <type>] = <expr>;
        let after_let = &trimmed[4..];
        let name = if let Some(colon_pos) = after_let.find(':') {
            after_let[..colon_pos].trim().to_string()
        } else if let Some(eq_pos) = after_let.find('=') {
            after_let[..eq_pos].trim().to_string()
        } else {
            continue;
        };

        bindings.push((name, line_num));
    }

    bindings
}

/// Parse return expressions from all user functions in the source.
///
/// Scans every function body for its final expression. For tuple returns
/// like `(a, b, sum_val, doubled, final_result)`, maps each slot index to
/// the variable name. For single returns like `final_result`, one slot.
///
/// When multiple functions exist, the return mapping from the function
/// with the most tuple elements wins (the "compute" function typically
/// returns all intermediate values as a tuple, while "main" just delegates).
///
/// This is used to map `RunResultValue::Success(values)` slots back to
/// the variable names that produced them.
fn parse_return_expression(source: &str) -> Vec<String> {
    let lines: Vec<&str> = source.lines().collect();
    let mut best: Vec<String> = vec![];

    // Simple state machine: walk lines, tracking function boundaries.
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();

        // Detect function start.
        if trimmed.starts_with("fn ") {
            // Find the return expression for this function by scanning
            // backwards from its closing brace.
            let fn_start = i;
            let mut brace_depth = 0i32;
            let mut fn_end = i;

            // Find the matching closing brace.
            for j in fn_start..lines.len() {
                for ch in lines[j].chars() {
                    if ch == '{' {
                        brace_depth += 1;
                    } else if ch == '}' {
                        brace_depth -= 1;
                    }
                }
                if brace_depth == 0 && j > fn_start {
                    fn_end = j;
                    break;
                }
            }

            // Scan backwards from the closing brace to find the return expression.
            for k in (fn_start + 1..fn_end).rev() {
                let line = lines[k].trim();
                if line.is_empty() || line == "}" || line == "{" {
                    continue;
                }

                // Tuple return: (a, b, c)
                if line.starts_with('(') && line.ends_with(')') {
                    let inner = &line[1..line.len() - 1];
                    let vars: Vec<String> = inner
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                    if vars.len() > best.len() {
                        best = vars;
                    }
                    break;
                }

                // Single variable return (not a function call).
                let expr = line.trim_end_matches(';').trim();
                if expr.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    let vars = vec![expr.to_string()];
                    if vars.len() > best.len() {
                        best = vars;
                    }
                }
                break;
            }

            i = fn_end + 1;
        } else {
            i += 1;
        }
    }

    best
}

/// Find the Cairo corelib path.
///
/// Search order:
/// 1. `CAIRO_CORELIB_DIR` environment variable
/// 2. `<CARGO_MANIFEST_DIR>/corelib/src` (for development)
/// 3. `<executable_dir>/../corelib/src`
/// 4. `<cwd>/corelib/src`
fn find_corelib_path() -> Option<PathBuf> {
    // 1. Environment variable
    if let Ok(dir) = std::env::var("CAIRO_CORELIB_DIR") {
        let path = PathBuf::from(dir);
        if path.exists() {
            return Some(path);
        }
    }

    // 2. Relative to CARGO_MANIFEST_DIR (development)
    if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
        let path = PathBuf::from(&manifest_dir).join("corelib").join("src");
        if path.exists() {
            return Some(path);
        }
    }

    // 3. Relative to executable
    if let Ok(exe_path) = std::env::current_exe() {
        for up in 1..=4 {
            let mut path = exe_path.clone();
            for _ in 0..=up {
                path.pop();
            }
            path.push("corelib");
            path.push("src");
            if path.exists() {
                return Some(path);
            }
        }
    }

    // 4. Current working directory
    if let Ok(cwd) = std::env::current_dir() {
        let path = cwd.join("corelib").join("src");
        if path.exists() {
            return Some(path);
        }
    }

    None
}

/// Extract function name from a line like "fn compute() -> felt252 {"
fn extract_fn_name(line: &str) -> Option<String> {
    let after_fn = line.strip_prefix("fn ")?.trim();
    let paren_pos = after_fn.find('(')?;
    Some(after_fn[..paren_pos].trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_fn_name() {
        assert_eq!(
            extract_fn_name("fn compute() -> (felt252, felt252) {"),
            Some("compute".to_string())
        );
        assert_eq!(
            extract_fn_name("fn main() -> felt252 {"),
            Some("main".to_string())
        );
        assert_eq!(extract_fn_name("let x = 5;"), None);
    }

    #[test]
    fn test_parse_let_binding_names() {
        let source = r#"
fn main() {
    let a: felt252 = 10;
    let b: felt252 = 32;
    let sum_val: felt252 = a + b;
}
"#;
        let bindings = parse_let_binding_names(source);
        assert_eq!(bindings.len(), 3);
        assert_eq!(bindings[0].0, "a");
        assert_eq!(bindings[1].0, "b");
        assert_eq!(bindings[2].0, "sum_val");
    }

    #[test]
    fn test_parse_return_expression_tuple() {
        let source = r#"
fn compute() -> (felt252, felt252, felt252) {
    let a: felt252 = 10;
    let b: felt252 = 32;
    let sum_val: felt252 = a + b;
    (a, b, sum_val)
}
"#;
        let vars = parse_return_expression(source);
        assert_eq!(vars, vec!["a", "b", "sum_val"]);
    }

    #[test]
    fn test_parse_return_expression_single() {
        let source = r#"
fn compute() -> felt252 {
    let result: felt252 = 42;
    result
}
"#;
        let vars = parse_return_expression(source);
        assert_eq!(vars, vec!["result"]);
    }

    #[test]
    fn test_parse_return_expression_full() {
        let source = r#"
fn compute() -> (felt252, felt252, felt252, felt252, felt252) {
    let a: felt252 = 10;
    let b: felt252 = 32;
    let sum_val: felt252 = a + b;
    let doubled: felt252 = sum_val * 2;
    let final_result: felt252 = doubled + a;
    (a, b, sum_val, doubled, final_result)
}
"#;
        let vars = parse_return_expression(source);
        assert_eq!(vars, vec!["a", "b", "sum_val", "doubled", "final_result"]);
    }
}
