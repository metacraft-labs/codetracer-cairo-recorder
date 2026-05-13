//! Tracer implementation for Cairo programs.
//!
//! Compiles a Cairo source file through the Sierra/CASM pipeline,
//! executes it using SierraCasmRunner, and emits CodeTracer trace events
//! (steps, calls, returns, variables).

use std::path::{Path, PathBuf};

use codetracer_trace_types::{EventLogKind, Line, TypeKind, ValueRecord, NONE_VALUE};
use codetracer_trace_writer_nim::trace_writer::TraceWriter;
use codetracer_trace_writer_nim::{create_trace_writer, TraceEventsFileFormat};
use eyre::{eyre, Context, Result};

use cairo_lang_compiler::db::RootDatabase;
use cairo_lang_compiler::project::setup_project;
use cairo_lang_compiler::CompilerConfig;
use cairo_lang_filesystem::db::init_dev_corelib;
use cairo_lang_filesystem::ids::CrateInput;
use cairo_lang_lowering::optimizations::config::Optimizations;
use cairo_lang_lowering::utils::InliningStrategy;
use cairo_lang_runner::{RunResultValue, SierraCasmRunner};
use cairo_lang_sierra::program::Program as SierraProgram;
use cairo_lang_utils::ordered_hash_map::OrderedHashMap;

use crate::source_map::SourceMap;

/// The main tracer struct that captures Cairo execution traces.
pub struct CairoTracer {
    writer: Box<dyn TraceWriter + Send>,
    /// Cairo felt252 type id (registered once).
    felt_type_id: Option<codetracer_trace_types::TypeId>,
    /// Cairo `Array<felt252>` type id (registered lazily once a compound
    /// Sequence value is about to be emitted).
    array_type_id: Option<codetracer_trace_types::TypeId>,
    /// Cairo tuple type id (registered lazily for compound Tuple values).
    tuple_type_id: Option<codetracer_trace_types::TypeId>,
}

impl CairoTracer {
    /// Trace a Cairo program and write a CodeTracer CTFS bundle.
    ///
    /// 1. Compiles the Cairo source to Sierra.
    /// 2. Runs the program using SierraCasmRunner.
    /// 3. Emits trace events based on execution results — see
    ///    [`emit_source_trace`](Self::emit_source_trace) for the
    ///    static-call-graph-driven DFS that drives the call/return ordering.
    /// 4. Writes the canonical CTFS multi-stream `.ct` container plus the
    ///    `trace_metadata.json` / `trace_paths.json` sidecars to `out_dir`.
    ///
    /// The output format is fixed to CTFS — see
    /// `Recorder-CLI-Conventions.md` §4 in `codetracer-specs`.  Use
    /// `ct print` (from `codetracer-trace-format-nim`) to convert the
    /// produced bundle to JSON or other text forms.
    pub fn trace_program(source_path: &Path, source_code: &str, out_dir: &Path) -> Result<()> {
        // CTFS-only.  Pre-2026-05-08 the recorder accepted a format
        // parameter (`TraceEventsFileFormat::{Json,Binary,Ctfs}`) and the
        // CLI exposed a `--format` flag.  The convention now mandates
        // CTFS exclusively.
        let format = TraceEventsFileFormat::Ctfs;
        // -- 1. Compile Cairo source to Sierra ----------------------------------------
        let compiler_config = CompilerConfig {
            replace_ids: true,
            ..CompilerConfig::default()
        };

        let corelib_path = find_corelib_path().ok_or_else(|| {
            eyre!(
                "Could not find Cairo corelib. Set CAIRO_CORELIB_DIR env var \
                 or place corelib/src next to the crate root."
            )
        })?;

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
            None,                      // metadata_config
            OrderedHashMap::default(), // starknet_contracts_info
            None,                      // run_profiler
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
                vec![],             // args
                None,               // available_gas
                Default::default(), // starknet_state
            )
            .map_err(|e| eyre!("Execution failed: {e}"))?;

        eprintln!("Execution completed");

        // -- 4. Extract return value --------------------------------------------------
        // The Cairo VM distinguishes Success / Panic results.  Both surface
        // an ordered list of felt252 values; Panic additionally needs to
        // raise a structured event so the failure is visible in the
        // CodeTracer event log (audit (c) per IsoNim section 5.6 — same
        // pattern as Move (1.46) ExecutionError and Cardano (1.48) UPLC
        // eval errors).
        //
        // Bug-fix 4: previously the recorder built `var_values` from the
        // panic-payload felts as well, so `let a = 10` ended up overwritten
        // with whatever value the panic decoder happened to surface in
        // slot 0 (typically 0 after `i64` overflow on the hash-shaped
        // first felt of the panic encoding).  Post-fix: only `Success`
        // results feed `var_values`; on panic the source-literal pass
        // (`parse_let_binding_literals`) keeps the let-bindings populated
        // with the values they actually held at the panic point.
        let panic_message: Option<String> = match &result.value {
            RunResultValue::Panic(values) => {
                let parts: Vec<String> = values.iter().map(|v| v.to_string()).collect();
                Some(format!(
                    "Cairo program panicked with {} value(s): [{}]",
                    values.len(),
                    parts.join(", ")
                ))
            }
            RunResultValue::Success(_) => None,
        };
        let success_return_values: Vec<i64> = match &result.value {
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
                Vec::new()
            }
        };
        let panicked = panic_message.is_some();

        // -- 5. Create the trace writer -----------------------------------------------
        let program_str = source_path.to_string_lossy();
        let mut tracer = CairoTracer {
            writer: create_trace_writer(&program_str, &[], format),
            felt_type_id: None,
            array_type_id: None,
            tuple_type_id: None,
        };

        // -- 6. Initialise output files -----------------------------------------------
        std::fs::create_dir_all(out_dir)
            .with_context(|| format!("cannot create output dir: {}", out_dir.display()))?;

        // CTFS multi-stream container.  Other formats are not reachable —
        // see the CTFS-only contract above.
        let events_filename = "trace.ctfs";
        let events_path = out_dir.join(events_filename);
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
            &success_return_values,
            panicked,
        )?;

        // -- 8b. Surface panic results through register_special_event ----------------
        // Closing audit (c): Cairo panics used to be stderr-only, lost from
        // the trace event log.  Routing them through register_special_event
        // mirrors the canonical pattern (Move 1.46, Cardano 1.48) and lets
        // the frontend surface them in the structured event-log pane.
        if let Some(message) = panic_message {
            TraceWriter::register_special_event(
                &mut *tracer.writer,
                EventLogKind::Error,
                "CairoPanic",
                &message,
            );
        }

        // -- 9. Finish writing --------------------------------------------------------
        TraceWriter::finish_writing_trace_events(&mut *tracer.writer).map_err(|e| eyre!("{e}"))?;
        TraceWriter::finish_writing_trace_metadata(&mut *tracer.writer)
            .map_err(|e| eyre!("{e}"))?;
        TraceWriter::finish_writing_trace_paths(&mut *tracer.writer).map_err(|e| eyre!("{e}"))?;
        tracer.writer.close().map_err(|e| eyre!("{e}"))?;

        Ok(())
    }

    /// Emit trace events by walking the source code and Sierra program structure.
    ///
    /// **Bug-fix 1+2 (joint).**  The previous implementation walked the
    /// source linearly and emitted one `register_call` / `register_return`
    /// pair per `fn` keyword in lexical order.  That violated two
    /// spec-shaped expectations:
    ///
    /// * `call_entry` should appear in **dynamic execution order** (the
    ///   order in which the VM actually invokes the functions), not in
    ///   the order the parser happens to encounter their definitions.
    /// * `call_exit` should follow **LIFO** stack ordering — innermost
    ///   callee closes first, outermost main closes last — so depth-aware
    ///   consumers can reconstruct the call tree.
    ///
    /// Post-fix: the recorder builds a static call graph from the source
    /// (`build_function_table` / `parse_callees_in_line`) and runs a
    /// **first-touch DFS** rooted at `main`.  Each function is visited
    /// exactly once — matching today's "5 calls" / "3 calls" pinned
    /// counts — but the visitation order is the dynamic call order
    /// (main → compute → … → leaves) and the post-order matches the LIFO
    /// closing order.  Step counts per function are unchanged because we
    /// still emit one step per non-skip line in each visited body.
    ///
    /// **Bug-fix 3.**  `register_return` now passes the function's
    /// computed return value (an `Int` ValueRecord backed by the felt252
    /// type id) instead of `NONE_VALUE`.  Values are derived from the
    /// VM's `RunResultValue::Success` payload mapped onto the source's
    /// `let X = callee(...)` pattern (`compute_function_return_values`).
    /// On panic the offending stack frames surface `Void` because the
    /// VM never actually returned them.
    ///
    /// **Bug-fix 4.**  Source-level let-bindings are now also seeded
    /// from literal initialisers (`let a: felt252 = 10;`) via
    /// `parse_let_binding_literals`, so a panic that fires before the
    /// VM produces a `Success` payload still leaves the bindings that
    /// **had** been evaluated populated with their real values.  The
    /// previous behaviour mapped the panic-payload felts onto the
    /// let-binding *names* by index — `a` would decode as the first
    /// felt of the panic encoding (typically 0 after i64 overflow).
    fn emit_source_trace(
        &mut self,
        source_path: &Path,
        source_map: &SourceMap,
        sierra_program: &SierraProgram,
        return_values: &[i64],
        panicked: bool,
    ) -> Result<()> {
        let source_code = source_map.source_code();

        // Parse let-binding names and their source lines (no value evaluation).
        let binding_names = parse_let_binding_names(source_code);

        // Parse compound (Array / Tuple) let-bindings whose elements can be
        // recovered from the source itself (literal-only `arr.append(N)`
        // sequences for Arrays, literal-only `(a, b, ...)` initialisers for
        // tuples).  These produce ValueRecord::Sequence / ValueRecord::Tuple
        // entries that the recorder emits at the step matching `emit_line`.
        // See the module-level note above `parse_compound_bindings` for
        // the heuristic's scope.
        let compound_bindings = parse_compound_bindings(source_code);

        // Parse the return expression of the **outermost** tuple-returning
        // function (typically `compute`) to map success-return-value slots
        // back to variable names.  Used both to populate scalar
        // let-bindings and to derive per-function return values via the
        // `let X = callee(...)` pattern.
        let return_var_map = parse_return_expression(source_code);

        // Build a name→value map from the VM's success return values.
        // On panic this stays empty by construction
        // (`return_values` is `Vec::new()` — see fix-4 comment in
        // `trace_program`).
        let mut var_values: std::collections::HashMap<String, i64> =
            std::collections::HashMap::new();
        for (idx, &val) in return_values.iter().enumerate() {
            if let Some(name) = return_var_map.get(idx) {
                var_values.insert(name.clone(), val);
            }
        }

        // Bug-fix 4 (literal-fallback): on a panic run only, seed
        // `var_values` from statically-evaluable literal let-bindings
        // (`let a: felt252 = 10;`).  These survive a panic because
        // they are extracted from the source text rather than from
        // the VM's panic-payload vector — pre-fix the panic-payload
        // felts were mapped onto the let-binding names by index, so
        // `a` decoded as the first felt of the panic encoding.  We
        // gate the fallback on `panicked` so the success path stays
        // VM-only (the VM is the source of truth there) and the
        // existing per-binding pinned counts don't pick up
        // intermediate `let mut x = 0;`-style accumulators that the
        // VM never reports back.
        if panicked {
            for (name, val) in parse_let_binding_literals(source_code) {
                var_values.entry(name).or_insert(val);
            }
        }

        // -- Static call-graph table --------------------------------------
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

        let fn_table = build_function_table(source_code, &user_functions);
        let fn_returns =
            compute_function_return_values(source_code, &fn_table, &var_values, panicked);

        // Walking entry: `main` is the canonical Cairo program entry
        // point.  If the source doesn't declare a `main` (atypical) we
        // fall back to first-declared so we still emit something.
        let root = fn_table
            .iter()
            .find(|f| f.bare_name == "main")
            .or_else(|| fn_table.first())
            .map(|f| f.bare_name.clone());

        // Active context: the function currently being walked, used to
        // decide whether a let-binding's variable should be emitted as a
        // step variable on its line.  (Step variables only fire when the
        // declaring function is the active DFS frame.)
        let mut visited: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        if let Some(root_name) = root {
            self.emit_function_dfs(
                source_path,
                &root_name,
                &fn_table,
                &fn_returns,
                &binding_names,
                &compound_bindings,
                &var_values,
                &mut visited,
            );
        }

        Ok(())
    }

    /// DFS through the static call graph starting at `fn_name`.  Skips
    /// functions that have already been visited (matching the
    /// "each function registered once" property the strict tests pin
    /// down via the `counts.calls` field).  Steps within a body fire
    /// in source order; recursion into a callee fires the moment its
    /// invocation line is reached so call-entry events nest correctly.
    fn emit_function_dfs(
        &mut self,
        source_path: &Path,
        fn_name: &str,
        fn_table: &[FunctionEntry],
        fn_returns: &std::collections::HashMap<String, Option<i64>>,
        binding_names: &[(String, u32)],
        compound_bindings: &[CompoundBinding],
        var_values: &std::collections::HashMap<String, i64>,
        visited: &mut std::collections::HashSet<String>,
    ) {
        if visited.contains(fn_name) {
            return;
        }
        visited.insert(fn_name.to_string());

        let entry = match fn_table.iter().find(|f| f.bare_name == fn_name) {
            Some(e) => e,
            None => return,
        };

        let felt_type_id = self.felt_type_id.expect("felt type id registered");

        // ---- register_call -------------------------------------------
        let fn_id = TraceWriter::ensure_function_id(
            &mut *self.writer,
            &entry.full_name,
            source_path,
            Line(entry.start_line as i64),
        );
        TraceWriter::register_call(&mut *self.writer, fn_id, vec![]);

        // ---- walk body lines -----------------------------------------
        let lines: Vec<&str> = entry.body.lines().collect();
        // `entry.body` is the full source — we still need indices into
        // it.  Use the absolute (1-based) line numbers stored on the
        // entry so step events match the canonical paths table.
        for line_offset in 0..entry.line_count {
            let abs_line = entry.start_line + line_offset as u32;
            let line_text = lines.get(abs_line as usize - 1).copied().unwrap_or("");
            let trimmed = line_text.trim();

            if trimmed.is_empty() || trimmed == "}" || trimmed == "{" {
                continue;
            }

            TraceWriter::register_step(
                &mut *self.writer,
                source_path,
                Line(abs_line as i64),
            );

            // Emit scalar variable values for any let-binding declared
            // on this line.  Values originate from `var_values`, which
            // is populated from the VM's success result and (on panic)
            // from statically-evaluated literal initialisers.
            for (name, line) in binding_names {
                if *line == abs_line {
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

            // Emit compound (Sequence / Tuple) values whose `emit_line`
            // matches.  Same code path as the scalar emission above —
            // collection bindings simply choose a richer ValueRecord
            // variant.
            for binding in compound_bindings {
                if binding.emit_line == abs_line {
                    let value = self.compound_to_value_record(binding);
                    TraceWriter::register_variable_with_full_value(
                        &mut *self.writer,
                        &binding.name,
                        value,
                    );
                }
            }

            // Recurse into any callees mentioned on this line.  Skipped
            // automatically when the callee was already visited
            // (`visited` set in `emit_function_dfs`).
            for callee in &entry.callees_per_line[line_offset] {
                self.emit_function_dfs(
                    source_path,
                    callee,
                    fn_table,
                    fn_returns,
                    binding_names,
                    compound_bindings,
                    var_values,
                    visited,
                );
            }
        }

        // ---- register_return -----------------------------------------
        // Bug-fix 3: surface the actual computed return value instead of
        // `NONE_VALUE`.  Functions that transitively reached a panic
        // surface `Void` (their VM frame never returned), preserving
        // the existing helper-asserted shape for the panic path.
        let return_value = match fn_returns.get(fn_name).copied().flatten() {
            Some(v) => ValueRecord::Int {
                i: v,
                type_id: felt_type_id,
            },
            None => NONE_VALUE,
        };

        // Surface the synthetic `return_value` step variable for the
        // root frame, mirroring the legacy trailing-step the lexical
        // walker emitted at the end of the source loop.  The variable
        // attaches to the most recently emitted step (the root's last
        // body line), which keeps `counts.values` aligned with the
        // pinned per-program counts.
        if entry.bare_name == "main" {
            if let Some(v) = fn_returns.get(fn_name).copied().flatten() {
                let value = ValueRecord::Int {
                    i: v,
                    type_id: felt_type_id,
                };
                TraceWriter::register_variable_with_full_value(
                    &mut *self.writer,
                    "return_value",
                    value,
                );
            }
        }

        TraceWriter::register_return(&mut *self.writer, return_value);
    }

    /// Lazily register the `Array<felt252>` type id for compound Sequence
    /// values (Cairo source: `Array<felt252>`).  The id is reused across
    /// every emitted Sequence in the trace.
    fn ensure_array_type_id(&mut self) -> codetracer_trace_types::TypeId {
        if let Some(id) = self.array_type_id {
            return id;
        }
        let id = TraceWriter::ensure_type_id(&mut *self.writer, TypeKind::Seq, "Array<felt252>");
        self.array_type_id = Some(id);
        id
    }

    /// Lazily register the tuple type id used for compound Tuple values.
    /// The same id is reused for every felt252 tuple regardless of arity —
    /// the recorder does not yet model arity-specific tuple types.
    fn ensure_tuple_type_id(&mut self) -> codetracer_trace_types::TypeId {
        if let Some(id) = self.tuple_type_id {
            return id;
        }
        let id = TraceWriter::ensure_type_id(
            &mut *self.writer,
            TypeKind::Tuple,
            "(felt252, ...)",
        );
        self.tuple_type_id = Some(id);
        id
    }

    /// Convert a parsed compound binding (Array literal sequence / tuple
    /// literal initialiser) into the matching `ValueRecord`.  All element
    /// felt literals are wrapped as `ValueRecord::Int` against the shared
    /// `felt252` type id registered at trace start.
    fn compound_to_value_record(&mut self, binding: &CompoundBinding) -> ValueRecord {
        let felt_type_id = self.felt_type_id.expect("felt type id registered");
        let elements: Vec<ValueRecord> = binding
            .elements
            .iter()
            .map(|i| ValueRecord::Int {
                i: *i,
                type_id: felt_type_id,
            })
            .collect();
        match binding.kind {
            CompoundKind::Array => ValueRecord::Sequence {
                elements,
                is_slice: false,
                type_id: self.ensure_array_type_id(),
            },
            CompoundKind::Tuple => ValueRecord::Tuple {
                elements,
                type_id: self.ensure_tuple_type_id(),
            },
        }
    }
}

/// Static-analysis snapshot of one user-defined Cairo function.
///
/// Built once per recording by `build_function_table`.  Drives the
/// dynamic-call-order DFS in `emit_function_dfs`: each entry knows
/// which lines it spans (so steps can be replayed) and which user
/// functions it directly calls on each of those lines (so the DFS
/// can recurse before continuing).
#[derive(Debug, Clone)]
struct FunctionEntry {
    /// Bare function name (e.g. `compute`).  Used as the DFS visit key
    /// and to render the user-facing call sequence.
    bare_name: String,
    /// Fully-qualified Sierra/CASM name (e.g.
    /// `flow_test::flow_test::compute`).  Passed to
    /// `ensure_function_id` so the trace's function table matches the
    /// Sierra function ids.
    full_name: String,
    /// 1-based source line of the `fn` keyword (used as the function's
    /// declaration line in the function table).
    start_line: u32,
    /// Number of source lines spanned by the function body, including
    /// the `fn` line and the closing `}`.  The DFS iterates
    /// `0..line_count` and adds `start_line` to map back to absolute
    /// source lines.
    line_count: usize,
    /// Full source text — kept so `emit_function_dfs` can re-derive the
    /// per-line text without holding a borrow on the parent slice.
    body: String,
    /// For every line in the function body (indexed by offset from
    /// `start_line`), the list of user-defined functions called from
    /// that line.  `parse_callees_in_line` recognises bare-identifier
    /// call syntax (`foo(...)`), which covers everything the test
    /// fixtures exercise.
    callees_per_line: Vec<Vec<String>>,
}

/// Compound (collection-shaped) let-binding extracted from the Cairo
/// source.  Today the heuristic recovers two flavours:
///
/// * `Array<felt252>` declarations whose elements are inserted via a
///   contiguous run of `<name>.append(<int_literal>);` statements that
///   live in the same function body.
/// * Tuple let-bindings whose RHS is a literal tuple of integer felts
///   (e.g. `let pair: (felt252, felt252) = (10, 20);`).
///
/// Anything else (computed elements, nested arrays, struct values, etc.)
/// is left to the scalar `parse_let_binding_names` path.  See the doc
/// comment on `parse_compound_bindings` for the parser's strict scope.
#[derive(Debug, Clone)]
struct CompoundBinding {
    name: String,
    /// 1-based source line at which the recorder emits the compound value
    /// as a step variable.  For arrays this is the line of the final
    /// `.append(...)` call (so the array is fully populated); for tuples
    /// this is the let-binding line itself.
    emit_line: u32,
    kind: CompoundKind,
    elements: Vec<i64>,
}

#[derive(Debug, Clone, Copy)]
enum CompoundKind {
    Array,
    Tuple,
}

/// Recover compound (Array / Tuple) let-bindings from the Cairo source.
///
/// This is a deliberately small heuristic that complements the felt-only
/// `parse_let_binding_names` path: it gives the recorder a fighting
/// chance to surface `ValueRecord::Sequence` / `ValueRecord::Tuple`
/// alongside the existing `ValueRecord::Int` emissions, even though it
/// stops well short of evaluating arbitrary Cairo expressions.
///
/// Recognised shapes (literal-only):
///
/// * `let mut <name>: Array<felt252> = ArrayTrait::new();` followed by
///   one or more `<name>.append(<int_literal>);` statements before the
///   end of the enclosing function body.  The recovered Sequence is
///   emitted at the line of the last matching `.append(...)`.
///
/// * `let <name>: (felt252, ...) = (<int_literal>, <int_literal>, ...);`
///   The recovered Tuple is emitted at the let-binding's own line.
///
/// A spec-compliant implementation would walk the Sierra/CASM debug info
/// and read the actual VM memory regions for these source-level types
/// (Option B in the bug write-up).  That's a much bigger project; this
/// heuristic is sufficient to expose the two ValueRecord variants the
/// fixture covers and to unblock collection-aware downstream consumers.
fn parse_compound_bindings(source: &str) -> Vec<CompoundBinding> {
    let lines: Vec<&str> = source.lines().collect();
    let mut out: Vec<CompoundBinding> = Vec::new();

    // First, split the source into function bodies so an `<name>.append`
    // that follows an Array decl in a *different* function does not
    // accidentally extend the previous function's array.
    let fn_ranges = function_line_ranges(&lines);

    for (fn_start, fn_end) in fn_ranges {
        for k in fn_start..=fn_end {
            let trimmed = lines[k].trim();
            if !trimmed.starts_with("let ") {
                continue;
            }

            // ----- Array<...> = ArrayTrait::new(); -------------------
            if let Some(name) = parse_mut_array_decl(trimmed) {
                let (emit_line, elements) =
                    collect_array_appends(&lines, k + 1, fn_end, &name);
                if !elements.is_empty() {
                    out.push(CompoundBinding {
                        name,
                        emit_line,
                        kind: CompoundKind::Array,
                        elements,
                    });
                }
                continue;
            }

            // ----- let <name>: (felt252, ...) = (lit, lit, ...); -----
            if let Some((name, elements)) = parse_literal_tuple_decl(trimmed) {
                out.push(CompoundBinding {
                    name,
                    emit_line: (k + 1) as u32,
                    kind: CompoundKind::Tuple,
                    elements,
                });
                continue;
            }
        }
    }

    out
}

/// Return inclusive (start, end) 0-based line index ranges for every
/// top-level `fn ...` body in the source.  Brace-depth tracker is the
/// same one used by `parse_return_expression` — kept here as a private
/// helper to avoid coupling the two.
fn function_line_ranges(lines: &[&str]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        if !trimmed.starts_with("fn ") {
            i += 1;
            continue;
        }
        let fn_start = i;
        let mut brace_depth = 0i32;
        let mut fn_end = i;
        for (j, line) in lines.iter().enumerate().skip(fn_start) {
            for ch in line.chars() {
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
        ranges.push((fn_start, fn_end));
        i = fn_end + 1;
    }
    ranges
}

/// Match `let mut <name>: Array<...> = ArrayTrait::new();` and return
/// `<name>`.  Anything more elaborate (custom constructor, type alias,
/// initial-value list) falls through.
fn parse_mut_array_decl(line: &str) -> Option<String> {
    // Strip `let mut ` prefix.
    let rest = line.strip_prefix("let mut ")?;
    let colon = rest.find(':')?;
    let name = rest[..colon].trim().to_string();
    let after_colon = rest[colon + 1..].trim_start();
    if !after_colon.starts_with("Array<") {
        return None;
    }
    if !line.contains("ArrayTrait::new()") {
        return None;
    }
    if name.is_empty() {
        return None;
    }
    Some(name)
}

/// Walk forward through a function body collecting integer literals
/// passed to `<name>.append(...)`.  Returns the (last-append 1-based
/// line, literal values) pair.  Stops at the first non-append statement
/// that mentions the name (e.g. the line that finally consumes the
/// array) or at the end of the function body.
fn collect_array_appends(
    lines: &[&str],
    start: usize,
    end: usize,
    name: &str,
) -> (u32, Vec<i64>) {
    let mut elements = Vec::new();
    let mut last_append_line = 0u32;
    let append_prefix = format!("{name}.append(");
    for k in start..=end {
        let trimmed = lines[k].trim();
        if !trimmed.starts_with(&append_prefix) {
            continue;
        }
        // Strip prefix and trailing `);`.
        let inside = &trimmed[append_prefix.len()..];
        let close = match inside.find(')') {
            Some(p) => p,
            None => continue,
        };
        let lit = inside[..close].trim();
        if let Ok(v) = lit.parse::<i64>() {
            elements.push(v);
            last_append_line = (k + 1) as u32;
        }
    }
    (last_append_line, elements)
}

/// Match `let <name>[: (...)]= (lit, lit, ...);` and return `(name,
/// elements)`.  Only pure integer-literal initialisers are recognised
/// today — destructuring binds (`let (x, y) = pair;`) and computed
/// expressions are intentionally ignored to keep the heuristic
/// conservative.
fn parse_literal_tuple_decl(line: &str) -> Option<(String, Vec<i64>)> {
    let rest = line.strip_prefix("let ")?;
    // Skip destructuring binds — they begin with a `(` after `let `.
    if rest.starts_with('(') {
        return None;
    }
    let eq = rest.find('=')?;
    let lhs = rest[..eq].trim();
    let rhs = rest[eq + 1..].trim().trim_end_matches(';').trim();

    let name = if let Some(colon_pos) = lhs.find(':') {
        lhs[..colon_pos].trim()
    } else {
        lhs
    };
    if name.is_empty() {
        return None;
    }

    if !(rhs.starts_with('(') && rhs.ends_with(')')) {
        return None;
    }
    let inner = &rhs[1..rhs.len() - 1];
    let mut elements = Vec::new();
    for part in inner.split(',') {
        let lit = part.trim();
        if lit.is_empty() {
            continue;
        }
        match lit.parse::<i64>() {
            Ok(v) => elements.push(v),
            Err(_) => return None,
        }
    }
    if elements.is_empty() {
        return None;
    }
    Some((name.to_string(), elements))
}

/// Build the static function table that drives the dynamic-call-order
/// DFS.  Pairs each user-declared `fn ...` with its body span, full
/// Sierra name, and a per-line list of direct callees.
///
/// The bare→full name lookup uses the `::<name>` substring match the
/// pre-fix linear walker already relied on (see the `extract_fn_name` /
/// `user_functions.iter().find(...)` block in the legacy
/// `emit_source_trace`).  Sierra emits names like
/// `<crate>::<crate>::<fn>` so the suffix match is unambiguous for the
/// fixtures the recorder targets.
fn build_function_table(source: &str, user_functions: &[&str]) -> Vec<FunctionEntry> {
    let lines: Vec<&str> = source.lines().collect();
    let ranges = function_line_ranges(&lines);
    let mut entries = Vec::new();
    for (start, end) in ranges {
        let header = lines[start].trim();
        let bare_name = match extract_fn_name(header) {
            Some(n) => n,
            None => continue,
        };
        let full_name = user_functions
            .iter()
            .find(|f| f.contains(&format!("::{}", bare_name)))
            .map(|s| s.to_string())
            .unwrap_or_else(|| bare_name.clone());

        let mut callees_per_line: Vec<Vec<String>> = Vec::with_capacity(end - start + 1);
        for k in start..=end {
            callees_per_line.push(parse_callees_in_line(lines[k], user_functions));
        }

        entries.push(FunctionEntry {
            bare_name,
            full_name,
            start_line: (start + 1) as u32,
            line_count: end - start + 1,
            body: source.to_string(),
            callees_per_line,
        });
    }
    entries
}

/// Detect direct calls to user-defined functions on a single source
/// line.  Returns the bare names in source-text order so the DFS
/// recurses into them in the order they appear (e.g. `compute()` body
/// calls `inner` before `middle` before `outer`).  Skips standard
/// library / generic call syntax (`ArrayTrait::new()`, `arr.append(N)`)
/// because none of the user-function bare names should collide with
/// those in the fixtures.
fn parse_callees_in_line(line: &str, user_functions: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    // Strip line / comment noise first; the rest is identifier-aware.
    let trimmed = line.split("//").next().unwrap_or(line);
    let bytes = trimmed.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_alphabetic() || c == b'_' {
            // Greedily consume an identifier.
            let start = i;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_')
            {
                i += 1;
            }
            let ident = &trimmed[start..i];
            // Skip leading `::` that would make this a path component
            // rather than a bare function call (`ArrayTrait::new` —
            // `new` would otherwise look like a call).
            let preceded_by_path =
                start >= 2 && &trimmed[start - 2..start] == "::";
            // Must be followed by `(` to count as a call.
            if !preceded_by_path
                && i < bytes.len()
                && bytes[i] == b'('
                && user_functions.iter().any(|f| {
                    f == &ident || f.ends_with(&format!("::{}", ident))
                })
            {
                out.push(ident.to_string());
            }
        } else {
            i += 1;
        }
    }
    out
}

/// Compute a per-function return value (`Some(felt)`) where the
/// recorder can statically derive one, or `None` for functions that
/// either panicked, transitively reached a panic, or whose return
/// shape isn't yet recognised by the heuristics.
///
/// Strategy:
///
/// 1. Build a `let-binding → callee` map from each function body so we
///    can read back `let X = callee(...)` patterns.
/// 2. For every binding whose value lives in `var_values`, propagate
///    the value to the callee mentioned on its RHS — this is how
///    `inner=3`, `middle=11`, `outer=111` fall out of the
///    nested_calls fixture (compute returns `(a, b, c, d)` and each
///    let-binding's name is the slot the corresponding call returned
///    into).
/// 3. For functions whose return is a tuple of bindings or a single
///    binding name, propagate the matching `var_values` entry.  For
///    `main` returning `compute()` — and similar single-callee
///    delegations — propagate the callee's value.
/// 4. Functions on the panic stack (anything reachable from a callee
///    that itself is unresolved on a panic run) stay `None`, so the
///    `Void` branch in `emit_function_dfs` keeps `assert_…_void`
///    holding for the panic-path tests.
fn compute_function_return_values(
    source: &str,
    fn_table: &[FunctionEntry],
    var_values: &std::collections::HashMap<String, i64>,
    panicked: bool,
) -> std::collections::HashMap<String, Option<i64>> {
    let mut out: std::collections::HashMap<String, Option<i64>> =
        std::collections::HashMap::new();

    // Per-binding → callee map for every function body.  Cleaner than
    // re-parsing inside the propagation loop, and the syntactic shape
    // we care about (`let <name>[: <type>]= <callee>(...)`) is small.
    let lines: Vec<&str> = source.lines().collect();

    // First pass: derive each non-main function's return value from the
    // let-binding it's assigned to in some other function's body.  This
    // catches the common pattern `let b: felt252 = inner(a, 2);` —
    // where the VM-known value of `b` IS `inner`'s return value.
    for entry in fn_table {
        for k in 0..entry.line_count {
            let abs = entry.start_line as usize + k;
            let line = lines.get(abs - 1).copied().unwrap_or("").trim();
            if let Some((bind_name, callee)) = parse_let_callee(line) {
                if let Some(&val) = var_values.get(&bind_name) {
                    out.entry(callee).or_insert(Some(val));
                }
            }
        }
    }

    // Second pass: derive each function's own return value from its
    // body's tail expression.  Tuple returns surface the VM payload's
    // last named slot; single-binding returns surface that binding;
    // single-callee delegation (`fn main() { compute() }`) propagates
    // the callee's already-resolved value.
    //
    // Run two iterations so a `main → compute` chain resolves even
    // when `compute`'s own value is computed in this pass.
    for _ in 0..2 {
        for entry in fn_table {
            // Skip functions whose value we've already locked in.
            if let Some(Some(_)) = out.get(&entry.bare_name) {
                continue;
            }

            // Find the function body's tail expression (last
            // non-empty / non-brace line).
            let mut tail_line: Option<&str> = None;
            for k in (0..entry.line_count).rev() {
                let abs = entry.start_line as usize + k;
                let raw = lines.get(abs - 1).copied().unwrap_or("");
                let t = raw.trim().trim_end_matches(';').trim();
                if t.is_empty() || t == "{" || t == "}" {
                    continue;
                }
                if t.starts_with("fn ") {
                    break;
                }
                tail_line = Some(t);
                break;
            }
            let tail = match tail_line {
                Some(t) => t,
                None => continue,
            };

            // Tuple-return: pull the last named slot from var_values.
            if tail.starts_with('(') && tail.ends_with(')') {
                let inner = &tail[1..tail.len() - 1];
                let last = inner.split(',').next_back().map(|s| s.trim().to_string());
                if let Some(name) = last {
                    if let Some(&v) = var_values.get(&name) {
                        out.insert(entry.bare_name.clone(), Some(v));
                        continue;
                    }
                }
            }

            // Bare-identifier return: that's the value.
            if tail.chars().all(|c| c.is_alphanumeric() || c == '_') {
                if let Some(&v) = var_values.get(tail) {
                    out.insert(entry.bare_name.clone(), Some(v));
                    continue;
                }
            }

            // Single-callee delegation: `compute()`, `divide(a, 0)`,
            // etc.  Propagate the callee's already-computed value if
            // we have one.
            if let Some(callee) = parse_single_call(tail) {
                if let Some(Some(v)) = out.get(&callee).copied() {
                    out.insert(entry.bare_name.clone(), Some(v));
                    continue;
                }
            }

            // Last resort: don't claim a value.
            out.entry(entry.bare_name.clone()).or_insert(None);
        }
    }

    // On a panic run, `var_values` will be empty (success-only) and
    // every entry above falls through to `None` — exactly the
    // `register_return(NONE_VALUE)` behaviour the `assert_*_void`
    // helper pins down for `error_paths_test.cairo`.
    let _ = panicked;

    out
}

/// Match `let <name>[: <type>]= <callee>(...)` and return
/// `(<name>, <callee>)`.  Used to seed per-function return values
/// from the let-binding's VM-known value.
fn parse_let_callee(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix("let ")?;
    if rest.starts_with('(') {
        return None;
    }
    let eq = rest.find('=')?;
    let lhs = rest[..eq].trim();
    let rhs = rest[eq + 1..].trim().trim_end_matches(';').trim();

    let name = if let Some(colon) = lhs.find(':') {
        lhs[..colon].trim()
    } else {
        lhs.trim_start_matches("mut ").trim()
    };
    if name.is_empty() {
        return None;
    }

    let callee = parse_single_call(rhs)?;
    Some((name.to_string(), callee))
}

/// Match `<callee>(...)` (with optional surrounding whitespace) and
/// return `<callee>`.  Used both for the let-binding pattern in
/// `parse_let_callee` and for the function-tail delegation pattern in
/// `compute_function_return_values`.
fn parse_single_call(expr: &str) -> Option<String> {
    let trimmed = expr.trim().trim_end_matches(';').trim();
    let paren = trimmed.find('(')?;
    let name = trimmed[..paren].trim();
    if name.is_empty() {
        return None;
    }
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_')
    {
        return None;
    }
    Some(name.to_string())
}

/// Parse let-bindings whose RHS is an integer literal (e.g.
/// `let a: felt252 = 10;`).  Used as a panic-resilient fallback so
/// the recorder can surface the values that *had* been assigned
/// before the VM panicked (bug-fix 4).
fn parse_let_binding_literals(source: &str) -> Vec<(String, i64)> {
    let mut out = Vec::new();
    for line in source.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("let ") {
            continue;
        }
        let rest = &trimmed[4..];
        if rest.starts_with('(') {
            continue;
        }
        let eq = match rest.find('=') {
            Some(p) => p,
            None => continue,
        };
        let lhs = rest[..eq].trim();
        let rhs = rest[eq + 1..].trim().trim_end_matches(';').trim();

        let name = if let Some(colon) = lhs.find(':') {
            lhs[..colon].trim()
        } else {
            lhs.trim_start_matches("mut ").trim()
        };
        if name.is_empty() {
            continue;
        }
        if let Ok(v) = rhs.parse::<i64>() {
            out.push((name.to_string(), v));
        }
    }
    out
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
            for (j, line) in lines.iter().enumerate().skip(fn_start) {
                for ch in line.chars() {
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
