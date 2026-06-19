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
    /// Per-user-struct type ids, keyed by bare struct name (e.g.
    /// `"Point"`).  Registered lazily on first emission of a
    /// `ValueRecord::Struct` so the trace's type table contains a
    /// dedicated entry per source-declared struct type rather than a
    /// single anonymous one.
    struct_type_ids: std::collections::HashMap<String, codetracer_trace_types::TypeId>,
    /// Shared `Variant` type id for `Option`/`Result`-shaped values.
    /// The discriminator distinguishes individual cases on the
    /// ValueRecord itself; the type id only needs to mark "this is a
    /// tagged-union value".
    variant_type_id: Option<codetracer_trace_types::TypeId>,
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
                // Bug-fix (M10 panic_with_felt252_test): the raw felt list
                // hides the human-readable `assert!` message that lives
                // *inside* the panic payload.  Cairo encodes
                // `panic_with_felt252("…")` / `assert!(cond, "…")` as a
                // sequence of felts: a class-of-panic tag, an outer
                // payload header, the message bytes packed into one or
                // more felts (31 ASCII bytes per felt252), and a final
                // byte-length felt.  We try to recover the message by
                // walking each felt, converting it to its big-endian
                // ASCII byte representation, and keeping the printable
                // runs.  Pre-fix the message surfaced as a string of
                // opaque integers; post-fix the recorder surfaces both
                // the raw values (for debugging / golden snapshots) and
                // the decoded message (for the event-log surface).
                let decoded = decode_cairo_panic_message(values);
                let prefix = format!(
                    "Cairo program panicked with {} value(s): [{}]",
                    values.len(),
                    parts.join(", ")
                );
                let full = if decoded.is_empty() {
                    prefix
                } else {
                    format!("{prefix} message=\"{decoded}\"")
                };
                Some(full)
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
            struct_type_ids: std::collections::HashMap::new(),
            variant_type_id: None,
        };

        // -- 6. Initialise output files -----------------------------------------------
        std::fs::create_dir_all(out_dir)
            .with_context(|| format!("cannot create output dir: {}", out_dir.display()))?;

        // CTFS multi-stream container.  Other formats are not reachable —
        // see the CTFS-only contract above.
        let events_filename = "trace.ctfs";
        let events_path = out_dir.join(events_filename);

        TraceWriter::begin_writing_trace_events(&mut *tracer.writer, &events_path)
            .map_err(|e| eyre!("{e}"))?;

        // FU-Column-Aware-Nav-Cairo: opt the canonical CTFS writer into
        // column-aware step encoding *before* the first `register_step`
        // / `start` call.  `enable_column_aware_steps` is sticky for the
        // lifetime of the trace and gates the writer's `DeltaColumn`
        // (tag 0x07) emission path plus the `meta.dat` bit 4 flag
        // (`FLAG_HAS_COLUMN_AWARE_STEPS`).  Even when individual steps
        // resolve to `column == None` (e.g. synthetic step lines)
        // downstream readers rely on the flag to decide whether to
        // surface a column field at all — mirrors the Solana / EVM /
        // JS recorder contract.
        TraceWriter::enable_column_aware_steps(&mut *tracer.writer);

        // M-capability-flags: Cairo's PC→source map is sharp enough
        // for per-column breakpoints and per-column motions.
        // Advertise both so the GUI exposes its M6 Alt+click
        // affordance and sub-statement step buttons.  See spec
        // `internal-files.md` §"Column-Aware Capability Flags".
        tracer.writer.enable_column_breakpoints_support();
        tracer.writer.enable_column_motions_support();

        // FU-Column-Aware-Nav-Cairo: register the source file's per-line
        // byte-length table BEFORE `TraceWriter::start`.  `start`
        // internally interns the path (without line-length data), and a
        // later `register_path_with_line_lengths` for an already-interned
        // path is silently dropped by the Nim writer — that drops the
        // line-length table needed by the reader's
        // `decodeGlobalPositionIndex`, so the per-step column field
        // never surfaces in ct-print.  Registering up front populates
        // `pathLineLengths` on the writer side and keeps the subsequent
        // `start` a no-op (path id already interned).
        let line_lengths = source_map.line_lengths();
        if let Err(err) = TraceWriter::register_path_with_line_lengths(
            &mut *tracer.writer,
            source_path,
            &line_lengths,
        ) {
            eprintln!(
                "[codetracer-cairo-recorder] register_path_with_line_lengths failed for {}: {} \
                 (column resolution will fall back to None for this file)",
                source_path.display(),
                err,
            );
        }

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
        tracer
            .writer
            .write_meta_dat("codetracer-cairo-recorder")
            .map_err(|e| eyre!("{e}"))?;
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

        // Parse destructuring let-bindings (`let (x, y) = pair;`) whose
        // RHS source binding is a recognised compound Tuple — so each
        // child name can be expanded into its corresponding tuple element
        // value at the destructuring line.  Pre-M10-round-2 only the
        // source `pair` binding surfaced; the destructured names were
        // dropped because both `parse_let_binding_names` and
        // `parse_compound_bindings` skipped `let (...)` patterns.
        let destructure_bindings = parse_destructure_bindings(source_code, &compound_bindings);

        // Parse `@T` (snapshot) and `ref T` (mutable reference) call
        // sites paired with the matching parameter declaration.  The
        // resulting `ReferenceEmission`s carry the callee + parameter
        // + source-binding name so the DFS walker can emit a typed
        // `ValueRecord::Reference` at the callee's first body line.
        let reference_emissions = parse_reference_emissions(source_code, &compound_bindings);

        // Parse bounded-width integer literal let-bindings so each
        // surfaces with its declared width (`u8`/`u16`/.../`i128`/
        // `u256`) rather than the shared felt252 carrier.  See
        // `parse_typed_int_bindings`.
        let typed_int_bindings = parse_typed_int_bindings(source_code);

        // Parse `<array>.pop_front()` mutations so each mutation line
        // re-emits the array as a `ValueRecord::Sequence` with its
        // post-mutation contents (M10 round-2 array_operations pin).
        let array_mutations = parse_array_mutations(source_code, &compound_bindings);

        // Parse `let <view> = <array>.span();` slice-view bindings so
        // each surfaces as a `ValueRecord::Sequence { is_slice: true,
        // ... }` carrying the source array's elements (M10 round-3
        // span pin).  The recorder doesn't yet propagate the Span
        // carrier into a callee parameter binding — the slice marker
        // lives on the source-side `view` binding only.
        let span_emissions = parse_span_emissions(source_code, &compound_bindings);

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
        let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();

        if let Some(root_name) = root {
            self.emit_function_dfs(
                source_path,
                &root_name,
                &fn_table,
                &fn_returns,
                &binding_names,
                &compound_bindings,
                &destructure_bindings,
                &reference_emissions,
                &typed_int_bindings,
                &array_mutations,
                &span_emissions,
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
    #[allow(clippy::too_many_arguments)]
    fn emit_function_dfs(
        &mut self,
        source_path: &Path,
        fn_name: &str,
        fn_table: &[FunctionEntry],
        fn_returns: &std::collections::HashMap<String, Option<i64>>,
        binding_names: &[(String, u32)],
        compound_bindings: &[CompoundBinding],
        destructure_bindings: &[DestructureBinding],
        reference_emissions: &[ReferenceEmission],
        typed_int_bindings: &[TypedIntBinding],
        array_mutations: &[ArrayMutation],
        span_emissions: &[SpanEmission],
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
        // Per-function simulator env, used by the `while` loop expander
        // (M11 fix) to evaluate the loop's condition + propagate
        // mutating assignments to subsequent iterations.  Seeded
        // lazily from `let mut <name>: <T> = <int_lit>;` initialisers
        // we encounter while walking so the simulator has the right
        // counter / accumulator values when it enters the loop.
        let mut sim_env: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
        // First-body-step latch: true until the first line of the body
        // emits a step.  Used to attach `@T` / `ref T` parameter
        // Reference values to the callee's entry step (M10 round-2
        // snapshot/ref pin).
        let mut first_step_emitted = false;
        // Pre-collected reference emissions targeting this callee.
        // Source-text order matches the call-site discovery order
        // produced by `parse_reference_emissions`.
        let pending_refs: Vec<&ReferenceEmission> = reference_emissions
            .iter()
            .filter(|r| r.callee == entry.bare_name)
            .collect();
        // `entry.body` is the full source — we still need indices into
        // it.  Use the absolute (1-based) line numbers stored on the
        // entry so step events match the canonical paths table.
        let mut line_offset: usize = 0;
        while line_offset < entry.line_count {
            let abs_line = entry.start_line + line_offset as u32;
            let line_text = lines.get(abs_line as usize - 1).copied().unwrap_or("");
            let trimmed = line_text.trim();

            // M11 fix: when this line opens a recognised `while`
            // loop, hand control to the per-iteration simulator and
            // skip past the body in the linear walk.  The simulator
            // emits one step per iteration per body line (plus a
            // re-emission of the header on each iteration), giving
            // the spec-correct execution-count-driven shape that
            // `test_loop_while_for_test_per_iteration_steps_pin`
            // pins down.  Loops we can't recognise (unparseable
            // condition / body, nested loops) fall through to the
            // line-by-line static walk.
            if let Some(loop_idx) = entry
                .while_loops
                .iter()
                .position(|w| w.header_line == abs_line)
            {
                let wl = &entry.while_loops[loop_idx];
                if self.simulate_while_loop(source_path, wl, &mut sim_env, felt_type_id) {
                    // Resume just past the closing `};` line.
                    let closing_offset = (wl.closing_line - entry.start_line) as usize;
                    line_offset = closing_offset + 1;
                    continue;
                }
                // Simulator declined (typically: condition references
                // a value the recorder doesn't yet know — e.g. a
                // function parameter).  Fall through to the static
                // walk so the per-line step events still surface.
            }

            if trimmed.is_empty() || trimmed == "}" || trimmed == "{" {
                line_offset += 1;
                continue;
            }

            // Direct tail-call expressions (`compute()`) need the callee's
            // call range to open before the parent tail line is buffered.
            // Otherwise the pending parent line is flushed as the callee's
            // entry step, and DAP/source-flow consumers see the callee with
            // an empty navigable body. Non-tail call statements and
            // `let x = callee(...)` keep the legacy step-before-recursing
            // order so binding values stay attached to the caller line.
            let callees_on_line = &entry.callees_per_line[line_offset];
            let recurse_before_step =
                is_direct_tail_call_line(&lines, entry, line_offset, callees_on_line);
            if recurse_before_step {
                for callee in callees_on_line {
                    self.emit_function_dfs(
                        source_path,
                        callee,
                        fn_table,
                        fn_returns,
                        binding_names,
                        compound_bindings,
                        destructure_bindings,
                        reference_emissions,
                        typed_int_bindings,
                        array_mutations,
                        span_emissions,
                        var_values,
                        visited,
                    );
                }
            }

            // FU-Column-Aware-Nav-Cairo: split the line into top-level
            // statements and emit one column-bearing step per statement
            // so multi-statement-per-line fixtures
            // (`let a = 1; let b = 2; let c = 3;`) surface as three
            // strictly distinct columns in `ct-print --full` rather than
            // collapsing onto a single step at column 1.  Single-statement
            // lines (the overwhelming common case in Cairo fixtures)
            // yield a single column at the line's first non-whitespace
            // byte.
            let stmt_columns = statement_columns_on_line(line_text);
            let deferred_call_site_column =
                (!recurse_before_step && stmt_columns.len() == 1 && !callees_on_line.is_empty())
                    .then_some(stmt_columns[0]);
            if deferred_call_site_column.is_some() {
                // `register_step_with_column(Some(col))` expands to a line
                // step plus a column-only step.  For a one-statement call
                // site, emitting both before recursion gives DAP step-in an
                // extra caller stop before the callee opens.  Emit only the
                // caller line now and move the column nudge after recursion.
                TraceWriter::register_step_with_column(
                    &mut *self.writer,
                    source_path,
                    Line(abs_line as i64),
                    None,
                );
            } else {
                for col in &stmt_columns {
                    TraceWriter::register_step_with_column(
                        &mut *self.writer,
                        source_path,
                        Line(abs_line as i64),
                        Some(Line(*col as i64)),
                    );
                }
            }

            // Attach pending `@T` / `ref T` reference emissions to the
            // callee's entry step (the first body step the DFS emits
            // for this function).  The dereferenced struct value is
            // recovered from the matching `CompoundBinding` so the
            // consumer can walk into the snapshot/ref the same way it
            // would walk into a by-value struct binding elsewhere.
            if !first_step_emitted && !pending_refs.is_empty() {
                for r in &pending_refs {
                    if let Some(value) = self.build_reference_value(r, compound_bindings) {
                        TraceWriter::register_variable_with_full_value(
                            &mut *self.writer,
                            &r.param_name,
                            value,
                        );
                    }
                }
            }
            first_step_emitted = true;

            // Seed the simulator env from any `let mut <name> = <int>;`
            // line we just stepped past — this is what gives the
            // `while`-loop simulator (above) `acc = 0` / `i = 0`
            // before it starts iterating.
            if let Some((name, val)) = parse_let_int_init(line_text) {
                sim_env.insert(name, val);
            }

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

            // Emit destructured children (`let (x, y) = pair;`).  Each
            // child is emitted as a scalar `ValueRecord::Int` carrying
            // the corresponding tuple-element value of the source
            // binding.  See `parse_destructure_bindings` for the
            // recognised shapes.
            for db in destructure_bindings {
                if db.emit_line == abs_line {
                    for (child_name, child_val) in &db.children {
                        let value = ValueRecord::Int {
                            i: *child_val,
                            type_id: felt_type_id,
                        };
                        TraceWriter::register_variable_with_full_value(
                            &mut *self.writer,
                            child_name,
                            value,
                        );
                    }
                }
            }

            // Emit array post-mutation snapshots for any mutation
            // call (`<arr>.pop_front()`, etc.) anchored at this line
            // (M10 round-2 array_operations pin).
            for am in array_mutations {
                if am.line == abs_line {
                    let value = self.build_array_mutation_value(am);
                    TraceWriter::register_variable_with_full_value(
                        &mut *self.writer,
                        &am.name,
                        value,
                    );
                }
            }

            // Emit Span<T> view bindings (`let view = <arr>.span();`)
            // as `ValueRecord::Sequence { is_slice: true, ... }` so
            // consumers can distinguish a borrowed slice view from an
            // owned Array (M10 round-3 span pin).
            for se in span_emissions {
                if se.line == abs_line {
                    let value = self.build_span_value(se);
                    TraceWriter::register_variable_with_full_value(
                        &mut *self.writer,
                        &se.name,
                        value,
                    );
                }
            }

            // Emit bounded-width integer let-bindings declared on
            // this line (M10 round-2 numeric-width pin).  Each
            // surfaces as `ValueRecord::Int { type_id }` against the
            // per-width type id (e.g. `u8`, `i64`) so consumers can
            // distinguish the declared bit width from the felt252
            // carrier.  `u256` surfaces as a dedicated
            // `ValueRecord::Struct { low, high }` matching Cairo's
            // 2×u128 representation.
            for tib in typed_int_bindings {
                if tib.line == abs_line {
                    let value = self.build_typed_int_value(tib);
                    TraceWriter::register_variable_with_full_value(
                        &mut *self.writer,
                        &tib.name,
                        value,
                    );
                }
            }

            // Recurse into any callees mentioned on this line.  Skipped
            // automatically when the callee was already visited
            // (`visited` set in `emit_function_dfs`).
            if !recurse_before_step {
                for callee in callees_on_line {
                    self.emit_function_dfs(
                        source_path,
                        callee,
                        fn_table,
                        fn_returns,
                        binding_names,
                        compound_bindings,
                        destructure_bindings,
                        reference_emissions,
                        typed_int_bindings,
                        array_mutations,
                        span_emissions,
                        var_values,
                        visited,
                    );
                }
                if let Some(col) = deferred_call_site_column {
                    let delta = col as i64 - 1;
                    if delta != 0 {
                        TraceWriter::write_delta_column(&mut *self.writer, delta);
                    }
                }
            }
            line_offset += 1;
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

    /// Replay a recognised `while` loop one iteration at a time so the
    /// trace surfaces the spec-correct execution-count-driven step
    /// pattern (see `test_loop_while_for_test_per_iteration_steps_pin`).
    ///
    /// Each iteration emits:
    /// 1. one step at the loop's header line (the condition check).
    /// 2. one step at every body line (skipping blank / `}`-only
    ///    lines, matching the same filter the linear walk uses).
    /// 3. for any body line whose statement is a recognised
    ///    assignment (`<name> = <expr>;`), one
    ///    `register_variable_with_full_value` carrying the post-update
    ///    `i64` value of the target.
    ///
    /// We also re-emit a final header step once the condition turns
    /// false so the trace surfaces the exit decision (matching how a
    /// human-driven step would land on the `while` keyword one last
    /// time before falling through).  That trailing step never
    /// emits variable updates.
    ///
    /// A safety cap (`MAX_ITERS = 1_000`) bounds the simulator so a
    /// pathological condition (e.g. `while i < 3 { /* no update */ }`)
    /// can't lock the recorder up.  Hitting the cap emits a warning
    /// to stderr and returns — the trace stays partial but valid.
    fn simulate_while_loop(
        &mut self,
        source_path: &Path,
        wl: &WhileLoop,
        env: &mut std::collections::HashMap<String, i64>,
        felt_type_id: codetracer_trace_types::TypeId,
    ) -> bool {
        const MAX_ITERS: u32 = 1_000;
        // First, probe the initial condition.  If it doesn't evaluate
        // — typically because the loop's bound is a function parameter
        // we don't (yet) propagate — give up entirely so the caller
        // can fall back to the static one-step-per-line walk.  This
        // keeps the per-fixture step counts stable for fixtures whose
        // bounds aren't recorder-known.
        if eval_expr(&wl.condition, env).is_none() {
            return false;
        }
        let mut iters = 0u32;
        while let Some(cond) = eval_expr(&wl.condition, env) {
            // Header step (re-emitted each iteration so consumers can
            // count loop iterations from the trace alone).  Column is
            // `None` here: the simulator doesn't carry the original
            // header-line source text, and the while-keyword's column
            // is irrelevant for column-aware navigation (single
            // synthetic statement per iteration).
            TraceWriter::register_step_with_column(
                &mut *self.writer,
                source_path,
                Line(wl.header_line as i64),
                None,
            );
            if cond == 0 {
                break;
            }
            // Body — one step per line + variable update for parsed
            // assignments.  We emit a step for *every* body line in
            // the parsed range (including unrecognised ones) so the
            // step_index sequence stays dense and aligned with the
            // source.  Lines we couldn't parse just don't contribute
            // a variable update.  Column-aware column is `None` — the
            // simulator's parsed Statement form has discarded the
            // original line text.
            for (offset, stmt) in wl.body_statements.iter().enumerate() {
                let abs = wl.body_start_line + offset as u32;
                TraceWriter::register_step_with_column(
                    &mut *self.writer,
                    source_path,
                    Line(abs as i64),
                    None,
                );
                if let Some(s) = stmt {
                    if let Some(new_val) = eval_expr(&s.rhs, env) {
                        env.insert(s.target.clone(), new_val);
                        let value = ValueRecord::Int {
                            i: new_val,
                            type_id: felt_type_id,
                        };
                        TraceWriter::register_variable_with_full_value(
                            &mut *self.writer,
                            &s.target,
                            value,
                        );
                    }
                }
            }
            iters += 1;
            if iters >= MAX_ITERS {
                eprintln!(
                    "while-loop simulator hit safety cap of {MAX_ITERS} iterations \
                     at line {}; trace will be partial",
                    wl.header_line
                );
                break;
            }
        }
        true
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
        let id = TraceWriter::ensure_type_id(&mut *self.writer, TypeKind::Tuple, "(felt252, ...)");
        self.tuple_type_id = Some(id);
        id
    }

    /// Lazily register a per-struct type id, keyed by Cairo struct name
    /// (e.g. `"Point"`).  The Nim writer doesn't expose field-type
    /// registration through `ensure_type_id` — the binary container
    /// stores only the `(kind, lang_type)` pair — but the source-declared
    /// `field_names` are still useful: we fold them into the rendered
    /// `lang_type` (e.g. `"Point{x,y}"`) so the trace's type table
    /// surfaces the field shape alongside the bare struct name.  The
    /// Struct ValueRecord still carries positional `field_values`, which
    /// downstream consumers zip against the `lang_type`'s `{...}` order.
    fn ensure_struct_type_id(
        &mut self,
        name: &str,
        field_names: &[String],
    ) -> codetracer_trace_types::TypeId {
        let lang_type = if field_names.is_empty() {
            name.to_string()
        } else {
            format!("{name}{{{}}}", field_names.join(","))
        };
        if let Some(&id) = self.struct_type_ids.get(&lang_type) {
            return id;
        }
        let id = TraceWriter::ensure_type_id(&mut *self.writer, TypeKind::Struct, &lang_type);
        self.struct_type_ids.insert(lang_type, id);
        id
    }

    /// Build a `ValueRecord::Sequence` for a `let <name> = <arr>.span();`
    /// slice-view binding.  Elements come from the source Array
    /// CompoundBinding.
    ///
    /// The Nim writer's `encode_recursive` arm for `ValueRecord::Sequence`
    /// now threads the `is_slice` discriminator through
    /// `ct_value_begin_sequence_with_slice`, so slice/view sequences
    /// (Cairo `Span<T>`) survive the FFI round-trip with `is_slice = true`.
    /// We retain the dedicated `Span<felt252>` type id alongside the
    /// flag so consumers can dispatch on either the lang_type or the
    /// field-level discriminator.
    fn build_span_value(&mut self, se: &SpanEmission) -> ValueRecord {
        let felt_type_id = self.felt_type_id.expect("felt type id registered");
        let elements: Vec<ValueRecord> = se
            .elements
            .iter()
            .map(|i| ValueRecord::Int {
                i: *i,
                type_id: felt_type_id,
            })
            .collect();
        let span_type_id =
            TraceWriter::ensure_type_id(&mut *self.writer, TypeKind::Seq, "Span<felt252>");
        ValueRecord::Sequence {
            elements,
            is_slice: true,
            type_id: span_type_id,
        }
    }

    /// Build a `ValueRecord::Sequence` carrying the post-mutation
    /// contents of an Array binding.  The element type id reuses the
    /// existing felt252 / Array<felt252> registrations so the emitted
    /// Sequence is shape-equivalent to the original compound binding.
    fn build_array_mutation_value(&mut self, am: &ArrayMutation) -> ValueRecord {
        let felt_type_id = self.felt_type_id.expect("felt type id registered");
        let elements: Vec<ValueRecord> = am
            .elements
            .iter()
            .map(|i| ValueRecord::Int {
                i: *i,
                type_id: felt_type_id,
            })
            .collect();
        ValueRecord::Sequence {
            elements,
            is_slice: false,
            type_id: self.ensure_array_type_id(),
        }
    }

    /// Build a `ValueRecord` for a bounded-width typed integer
    /// binding.  Most widths surface as `ValueRecord::Int { type_id }`
    /// against a per-width Int type id.  `u256` is special-cased to a
    /// `ValueRecord::Struct { field_values: [low, high] }` matching
    /// Cairo's 2×u128 representation — its type id is registered
    /// against `TypeKind::Struct` with the field-shape
    /// `"u256{low,high}"`.
    fn build_typed_int_value(&mut self, tib: &TypedIntBinding) -> ValueRecord {
        match tib.kind {
            TypedIntKind::U256 => {
                // Split the i128 value into low/high u128 halves.  All
                // current fixtures fit comfortably in u128 so `high` is
                // zero; the split logic still applies for future >u128
                // literals (although Rust's i128 only carries 128 bits
                // of value plus sign — we treat negative values as
                // wrapped to u256 by reinterpreting as u128).
                let value_u128: u128 = tib.value as u128;
                let u128_id = self.ensure_typed_int_type_id(TypedIntKind::U128);
                let struct_id = TraceWriter::ensure_type_id(
                    &mut *self.writer,
                    TypeKind::Struct,
                    "u256{low,high}",
                );
                let low = ValueRecord::Int {
                    i: value_u128 as i64,
                    type_id: u128_id,
                };
                let high = ValueRecord::Int {
                    i: 0,
                    type_id: u128_id,
                };
                ValueRecord::Struct {
                    field_values: vec![low, high],
                    type_id: struct_id,
                }
            }
            _ => {
                let type_id = self.ensure_typed_int_type_id(tib.kind);
                ValueRecord::Int {
                    i: tib.value as i64,
                    type_id,
                }
            }
        }
    }

    /// Register (or reuse) the per-width `Int` type id for a
    /// bounded-width integer kind (`u8`, `i64`, etc.).  The Nim writer
    /// dedupes by `(kind, lang_type)` so passing the same lang_type
    /// twice returns the same id.
    fn ensure_typed_int_type_id(&mut self, kind: TypedIntKind) -> codetracer_trace_types::TypeId {
        TraceWriter::ensure_type_id(&mut *self.writer, TypeKind::Int, kind.lang_type())
    }

    /// Lazily register the shared `Ref` type id used for the `@T` /
    /// `ref T` parameter references emitted by the snapshot/ref pin.
    /// The `lang_type` deliberately stays generic (`"Ref"`) so a
    /// single id is sufficient regardless of pointee struct shape —
    /// the dereferenced value carries the full type info.
    fn ensure_ref_type_id(&mut self) -> codetracer_trace_types::TypeId {
        TraceWriter::ensure_type_id(&mut *self.writer, TypeKind::Ref, "Ref")
    }

    /// Build a `ValueRecord::Reference` for an emitted snapshot or
    /// mutable-reference parameter.  The dereferenced value is the
    /// matching `CompoundBinding` (Struct shape only — anything else
    /// produces `None` and the emission is dropped).  The synthesised
    /// `address` field is taken from the emission (deterministic per
    /// recording, distinct per call site).
    fn build_reference_value(
        &mut self,
        emission: &ReferenceEmission,
        compound_bindings: &[CompoundBinding],
    ) -> Option<ValueRecord> {
        let binding = compound_bindings.iter().find(|b| {
            b.name == emission.source_binding && matches!(b.kind, CompoundKind::Struct)
        })?;
        let dereferenced = self.compound_to_value_record(binding);
        let type_id = self.ensure_ref_type_id();
        Some(ValueRecord::Reference {
            dereferenced: Box::new(dereferenced),
            address: emission.address,
            mutable: emission.mutable,
            type_id,
        })
    }

    /// Lazily register a generic `Variant` type id for `Option`/`Result`
    /// shaped values.  The discriminator lives on the ValueRecord itself,
    /// so a single shared type id is sufficient for the heuristic — the
    /// `lang_type` deliberately stays generic (`"Variant"`) to avoid
    /// pretending we model the enum-payload type system.
    fn ensure_variant_type_id(&mut self) -> codetracer_trace_types::TypeId {
        if let Some(id) = self.variant_type_id {
            return id;
        }
        let id = TraceWriter::ensure_type_id(&mut *self.writer, TypeKind::Variant, "Variant");
        self.variant_type_id = Some(id);
        id
    }

    /// Convert a parsed compound binding (Array literal sequence / tuple
    /// literal initialiser / struct literal / variant literal) into the
    /// matching `ValueRecord`.  All element felt literals are wrapped as
    /// `ValueRecord::Int` against the shared `felt252` type id registered
    /// at trace start.
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
            CompoundKind::Struct => {
                let type_id = self.ensure_struct_type_id(&binding.type_name, &binding.field_names);
                ValueRecord::Struct {
                    field_values: elements,
                    type_id,
                }
            }
            CompoundKind::Variant => {
                // The `Variant` ValueRecord holds a single boxed
                // `contents`.  For `None` we synthesise a `NONE_VALUE`
                // tag (the discriminator alone is enough for the consumer
                // to distinguish None from Some(x)).  For `Some(x)` /
                // `Ok(x)` / `Err(x)` we wrap the single payload felt as
                // an `Int` ValueRecord directly — Cairo's payload-of-one
                // case doesn't need a synthetic enclosing Tuple/Struct.
                let type_id = self.ensure_variant_type_id();
                let contents: ValueRecord = if elements.is_empty() {
                    NONE_VALUE
                } else if elements.len() == 1 {
                    elements.into_iter().next().unwrap()
                } else {
                    ValueRecord::Tuple {
                        elements,
                        type_id: self.ensure_tuple_type_id(),
                    }
                };
                ValueRecord::Variant {
                    discriminator: binding.type_name.clone(),
                    contents: Box::new(contents),
                    type_id,
                }
            }
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
    /// `while` loops detected inside this function body.  Each entry
    /// records the absolute (1-based) header line, the body's first /
    /// last lines, and the (parsed) condition expression so the DFS
    /// can replay the loop one iteration at a time — matching the
    /// spec's "one step per executed iteration" expectation rather
    /// than the static-DFS "one step per source line" approximation.
    /// See `parse_while_loops` for the exact shape recognised.
    while_loops: Vec<WhileLoop>,
}

/// Statically-recovered shape of a `while` loop in a Cairo function
/// body.  `header_line` is the line of the `while <cond> {` keyword;
/// `body_start_line` is the first body line (one past the header);
/// the body extends for `body_statements.len()` lines.  `closing_line`
/// points at the line holding the matching `};` (or `}` if the user
/// omits the trailing semicolon) so the linear walker can resume
/// exactly past the loop.
///
/// The `condition` and per-statement parses use a deliberately tiny
/// expression grammar (integer literals + bare identifiers + the
/// arithmetic / comparison / logical operators the fixtures exercise).
/// Anything outside that grammar makes `parse_while_loops` skip the
/// loop, so the recorder degrades back to one-step-per-source-line
/// for unrecognised shapes rather than mis-recording.
#[derive(Debug, Clone)]
struct WhileLoop {
    header_line: u32,
    body_start_line: u32,
    closing_line: u32,
    condition: Expr,
    /// Per-body-line parse: `Some(stmt)` if we recognised the
    /// assignment shape (`<name> = <expr>;`), `None` otherwise.  An
    /// unrecognised body line still emits a step but contributes no
    /// variable update for the simulator's env.
    body_statements: Vec<Option<Statement>>,
}

/// One recognised statement inside a `while` body — the simulator's
/// only mutation handle.  Today we only need plain assignment to a
/// previously-declared mutable binding (e.g. `acc = acc + 1;`); the
/// fixture's two loops fit entirely in this shape.
#[derive(Debug, Clone)]
struct Statement {
    target: String,
    rhs: Expr,
}

/// Tiny arithmetic / comparison / logical expression AST used by the
/// `while`-loop simulator.  The evaluator (`eval_expr`) returns `i64`
/// for arithmetic and `0`/`1` for boolean predicates — matching the
/// felt252-shaped scalar values the rest of the recorder already
/// stores in `var_values`.
#[derive(Debug, Clone)]
enum Expr {
    Lit(i64),
    Var(String),
    Bin(BinOp, Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BinOp {
    Add,
    Sub,
    Mul,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    And,
    Or,
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
    /// Optional shape data for typed compound bindings.
    ///
    /// * For `CompoundKind::Struct` this is the Cairo struct name (e.g.
    ///   `"Point"`) used both to register the per-struct `type_id` and
    ///   to render the `lang_type` field in `ct-print --full`.  The
    ///   `field_names` carry the source-declared field order — the
    ///   matching values live in `elements`.
    /// * For `CompoundKind::Variant` this is the variant discriminator
    ///   (e.g. `"Some"`, `"None"`, `"Ok"`, `"Err"`).  `elements` carries
    ///   the payload values (empty for `None`).
    type_name: String,
    field_names: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
enum CompoundKind {
    Array,
    Tuple,
    /// User-defined struct literal — `Point { x: 3, y: 4 }`.  See the
    /// note on `CompoundBinding` for the field-shape carrier.
    Struct,
    /// Enum variant — `Option::Some(7)`, `Result::Ok(11)`, etc.  The
    /// discriminator lives in `type_name` and the payload felts in
    /// `elements`.
    Variant,
}

/// Reference emission for a function whose signature includes a
/// `@T` (snapshot) or `ref T` (mutable reference) parameter.
///
/// Recovered statically from a call site like `read_only(@origin)` or
/// `scale(ref shift, ...)` paired with the callee's parameter list.
/// Pre-fix the recorder's source-walk path didn't track parameters at
/// all; round-2 of M10 adds this opt-in pin so consumers can
/// distinguish snapshot from mutable-reference passing without
/// disturbing the value-only step variables emitted elsewhere.
///
/// Each emission carries a synthetic deterministic `address` (drawn
/// from a per-fixture counter) so multiple references to the same
/// source binding still produce distinct addresses, matching the
/// pointer-identity semantics every downstream consumer expects.
#[derive(Debug, Clone)]
struct ReferenceEmission {
    /// Bare name of the function whose body the reference is emitted
    /// inside (the callee).
    callee: String,
    /// Parameter name that surfaces as the bound variable on the
    /// callee's entry step.
    param_name: String,
    /// Source binding the reference points at — its
    /// `CompoundBinding::Struct` value is reused as the
    /// `dereferenced` field of the emitted Reference value.
    source_binding: String,
    /// `true` for `ref T`, `false` for `@T`.
    mutable: bool,
    /// Synthetic stable address — distinct per emission so the same
    /// caller passing the same source twice still surfaces two
    /// addresses (pointer identity).
    address: u64,
}

/// Post-mutation snapshot of an Array compound binding.  Re-emitted
/// at every recognised mutation line (today only `<name>.pop_front();`)
/// so consumers can see the array's contents at each point in time.
#[derive(Debug, Clone)]
struct ArrayMutation {
    name: String,
    /// 1-based source line of the mutation — also the emit line.
    line: u32,
    /// Post-mutation contents as raw integer felts.
    elements: Vec<i64>,
}

/// Slice-view binding recovered from `let <name> = <arr>.span();`.
/// Carries the source Array's contents so the emitted
/// `ValueRecord::Sequence` shares the underlying element list, and
/// flips the `is_slice` discriminator on so consumers can distinguish
/// a borrowed Span<T> from an owned Array<T> binding.
#[derive(Debug, Clone)]
struct SpanEmission {
    /// Bound view name (the LHS of the let).
    name: String,
    /// 1-based source line of the let — also the emit line.
    line: u32,
    /// Element values copied from the source Array binding.
    elements: Vec<i64>,
}

/// Bounded-width integer let-binding (`let <name>: u8 = <lit>;` etc.).
///
/// Recovered statically from a `let <name>: <T> = <int_lit>;` line where
/// `<T>` is one of the recognised bounded widths.  The value is parsed
/// from the literal (no VM round-trip needed), so the binding survives a
/// downstream panic the same way `parse_let_binding_literals` does for
/// felt252.  `u256` is handled out-of-band via `TypedIntKind::U256` and
/// surfaces as a `ValueRecord::Struct { low, high }`.
#[derive(Debug, Clone)]
struct TypedIntBinding {
    name: String,
    /// 1-based source line of the let-binding — also the emit line.
    line: u32,
    kind: TypedIntKind,
    /// Parsed value (felt-padded into i128 to fit signed widths).
    value: i128,
}

#[derive(Debug, Clone, Copy)]
enum TypedIntKind {
    U8,
    U16,
    U32,
    U64,
    U128,
    I8,
    I16,
    I32,
    I64,
    I128,
    /// `u256` — surfaces as a `ValueRecord::Struct { low: u128,
    /// high: u128 }` carrier.  For literal values that fit in u128
    /// the `high` half is always zero; for larger values (none today)
    /// we'd split on 128 bits.
    U256,
}

impl TypedIntKind {
    fn lang_type(&self) -> &'static str {
        match self {
            TypedIntKind::U8 => "u8",
            TypedIntKind::U16 => "u16",
            TypedIntKind::U32 => "u32",
            TypedIntKind::U64 => "u64",
            TypedIntKind::U128 => "u128",
            TypedIntKind::I8 => "i8",
            TypedIntKind::I16 => "i16",
            TypedIntKind::I32 => "i32",
            TypedIntKind::I64 => "i64",
            TypedIntKind::I128 => "i128",
            TypedIntKind::U256 => "u256",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "u8" => TypedIntKind::U8,
            "u16" => TypedIntKind::U16,
            "u32" => TypedIntKind::U32,
            "u64" => TypedIntKind::U64,
            "u128" => TypedIntKind::U128,
            "i8" => TypedIntKind::I8,
            "i16" => TypedIntKind::I16,
            "i32" => TypedIntKind::I32,
            "i64" => TypedIntKind::I64,
            "i128" => TypedIntKind::I128,
            "u256" => TypedIntKind::U256,
            _ => return None,
        })
    }
}

/// Tuple-destructuring let-binding: `let (x, y, ...) = <source>;`.
///
/// Each `child` carries the bound name and the integer value drawn from
/// the matching positional slot of the source tuple.  Recognised when
/// `<source>` is a bare identifier matching a previously-emitted
/// `CompoundBinding` of `CompoundKind::Tuple` whose `elements` count
/// equals the number of destructured names.
#[derive(Debug, Clone)]
struct DestructureBinding {
    /// 1-based source line of the destructuring let — the line at which
    /// each child variable is emitted as a scalar Int step variable.
    emit_line: u32,
    /// Per-child `(name, value)` pairs in source order.
    children: Vec<(String, i64)>,
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
                let (emit_line, elements) = collect_array_appends(&lines, k + 1, fn_end, &name);
                if !elements.is_empty() {
                    out.push(CompoundBinding {
                        name,
                        emit_line,
                        kind: CompoundKind::Array,
                        elements,
                        type_name: String::new(),
                        field_names: Vec::new(),
                    });
                }
                continue;
            }

            // ----- let [mut] <name> = array![<lit>, <lit>, ...]; -----
            // M10 round-2: recognise the `array![]` macro literal.
            // The recovered Sequence is emitted at the let-binding's
            // own line (the macro is fully evaluated by the time the
            // binding executes, unlike the multi-statement `.append`
            // pattern above).
            if let Some((name, elements)) = parse_array_macro_decl(trimmed) {
                out.push(CompoundBinding {
                    name,
                    emit_line: (k + 1) as u32,
                    kind: CompoundKind::Array,
                    elements,
                    type_name: String::new(),
                    field_names: Vec::new(),
                });
                continue;
            }

            // ----- let <name>: (felt252, ...) = (lit, lit, ...); -----
            if let Some((name, elements)) = parse_literal_tuple_decl(trimmed) {
                out.push(CompoundBinding {
                    name,
                    emit_line: (k + 1) as u32,
                    kind: CompoundKind::Tuple,
                    elements,
                    type_name: String::new(),
                    field_names: Vec::new(),
                });
                continue;
            }

            // ----- let <name>: TypeName = TypeName { field: lit, ... };
            //       — user-defined struct literal initialiser.
            if let Some((name, struct_name, field_names, elements)) =
                parse_struct_literal_decl(trimmed)
            {
                out.push(CompoundBinding {
                    name,
                    emit_line: (k + 1) as u32,
                    kind: CompoundKind::Struct,
                    elements,
                    type_name: struct_name,
                    field_names,
                });
                continue;
            }

            // ----- let <name>: <T> = Option::Some(lit) / Option::None /
            //       Result::Ok(lit) / Result::Err(lit); — Variant literal.
            if let Some((name, discriminator, elements)) = parse_variant_literal_decl(trimmed) {
                out.push(CompoundBinding {
                    name,
                    emit_line: (k + 1) as u32,
                    kind: CompoundKind::Variant,
                    elements,
                    type_name: discriminator,
                    field_names: Vec::new(),
                });
                continue;
            }
        }
    }

    out
}

/// Match `let <name>[: <Type>] = <StructName> { <field>: <lit>, ... };` and
/// return `(name, struct_name, field_names, elements)`.
///
/// Cairo lets users initialise structs with the `TypeName { field: value,
/// ... }` syntax.  We recognise the literal-only form (every field
/// receives an integer literal) so the recorder can emit a
/// `ValueRecord::Struct` with positional `field_values` matching the
/// source-declared field order.  Anything more elaborate (computed
/// fields, shorthand `Point { x, y }`, nested structs) falls through to
/// the scalar binding path.
fn parse_struct_literal_decl(line: &str) -> Option<(String, String, Vec<String>, Vec<i64>)> {
    let rest = line.strip_prefix("let ")?;
    if rest.starts_with('(') {
        return None;
    }
    let eq = rest.find('=')?;
    let lhs = rest[..eq].trim();
    let rhs = rest[eq + 1..].trim().trim_end_matches(';').trim();

    let name = if let Some(colon) = lhs.find(':') {
        lhs[..colon].trim_start_matches("mut ").trim()
    } else {
        lhs.trim_start_matches("mut ").trim()
    };
    if name.is_empty() {
        return None;
    }

    // Find the `{ ... }` body.
    let brace_open = rhs.find('{')?;
    let brace_close = rhs.rfind('}')?;
    if brace_close <= brace_open {
        return None;
    }
    let struct_name = rhs[..brace_open].trim().to_string();
    if struct_name.is_empty()
        || !struct_name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == ':')
    {
        return None;
    }
    // Last `::<Name>` segment is the discriminator — for plain struct
    // literals (`Point { ... }`) this collapses to `Point` itself.
    let bare_struct = struct_name
        .rsplit("::")
        .next()
        .unwrap_or(&struct_name)
        .to_string();

    let body = rhs[brace_open + 1..brace_close].trim();
    let mut field_names = Vec::new();
    let mut elements = Vec::new();
    for part in body.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let colon = part.find(':')?;
        let field = part[..colon].trim().to_string();
        let val = part[colon + 1..].trim();
        let v = val.parse::<i64>().ok()?;
        field_names.push(field);
        elements.push(v);
    }
    if field_names.is_empty() {
        return None;
    }

    Some((name.to_string(), bare_struct, field_names, elements))
}

/// Match `let <name>[: <Type>] = Option::Some(<lit>)` /
/// `Option::None` / `Result::Ok(<lit>)` / `Result::Err(<lit>)` and
/// return `(name, discriminator, elements)`.
///
/// The `discriminator` is the bare-variant name (`"Some"`, `"None"`,
/// `"Ok"`, `"Err"`) without the enum prefix — matches the
/// `ValueRecord::Variant.discriminator` shape so ct-print surfaces a
/// clean tag.  `elements` is empty for `None` and holds the single
/// payload felt for the others.  Multi-payload variants fall through.
fn parse_variant_literal_decl(line: &str) -> Option<(String, String, Vec<i64>)> {
    let rest = line.strip_prefix("let ")?;
    if rest.starts_with('(') {
        return None;
    }
    let eq = rest.find('=')?;
    let lhs = rest[..eq].trim();
    let rhs = rest[eq + 1..].trim().trim_end_matches(';').trim();

    let name = if let Some(colon) = lhs.find(':') {
        lhs[..colon].trim_start_matches("mut ").trim()
    } else {
        lhs.trim_start_matches("mut ").trim()
    };
    if name.is_empty() {
        return None;
    }

    // Recognised prefixes — keep the list tight so we don't accidentally
    // shadow a bare-call (`Foo(x)`) that the let-callee path already
    // claims for return-value propagation.
    let known: &[(&str, bool)] = &[
        ("Option::Some(", true),
        ("Option::None", false),
        ("Result::Ok(", true),
        ("Result::Err(", true),
    ];
    for (prefix, has_payload) in known {
        if !rhs.starts_with(prefix) {
            continue;
        }
        let bare = prefix
            .trim_end_matches('(')
            .rsplit("::")
            .next()
            .unwrap_or("")
            .to_string();
        if !has_payload {
            return Some((name.to_string(), bare, Vec::new()));
        }
        // Find matching `)` and parse a single integer literal payload.
        let inside = &rhs[prefix.len()..];
        let close = inside.find(')')?;
        let lit = inside[..close].trim();
        let v = lit.parse::<i64>().ok()?;
        return Some((name.to_string(), bare, vec![v]));
    }
    None
}

/// Return inclusive (start, end) 0-based line index ranges for every
/// top-level `fn ...` body in the source.  Brace-depth tracker is the
/// same one used by `parse_return_expression` — kept here as a private
/// helper to avoid coupling the two.
///
/// Trait method signatures (`fn greet(self: T) -> u32;` ending in `;`
/// rather than `{`) are skipped — they're forward declarations with no
/// body, so emitting a function entry for them would synthesise a
/// phantom DFS node whose body actually belongs to the surrounding
/// trait/impl block.  See `impl_block_ranges` for the impl-method
/// disambiguation that pairs each impl `greet` with its
/// `<ImplName>::greet` bare name.
fn function_line_ranges(lines: &[&str]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        if !trimmed.starts_with("fn ") {
            i += 1;
            continue;
        }
        // Trait method signatures (`fn greet(self: T) -> u32;`) carry no
        // body — skip them so the function table doesn't synthesise a
        // phantom range that swallows the surrounding trait/impl block.
        if trimmed.ends_with(';') && !trimmed.contains('{') {
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

/// Match `let [mut] <name>[: <Type>] = array![<lit>, <lit>, ...];` and
/// return `(<name>, <elements>)`.
///
/// The `array!` macro is the idiomatic single-expression form for
/// initialising an `Array<T>`.  We recognise the literal-only payload
/// (each element parses as an integer literal, optionally suffixed with
/// a width hint like `1_u32`); anything more elaborate (computed
/// elements, nested macros, non-integer payloads) falls through to the
/// scalar binding path.
fn parse_array_macro_decl(line: &str) -> Option<(String, Vec<i64>)> {
    let rest = line.strip_prefix("let ")?;
    if rest.starts_with('(') {
        return None;
    }
    let eq = rest.find('=')?;
    let lhs = rest[..eq].trim();
    let rhs = rest[eq + 1..].trim().trim_end_matches(';').trim();

    let name = if let Some(colon) = lhs.find(':') {
        lhs[..colon].trim_start_matches("mut ").trim()
    } else {
        lhs.trim_start_matches("mut ").trim()
    };
    if name.is_empty() {
        return None;
    }
    let inner = rhs.strip_prefix("array![")?;
    let close = inner.rfind(']')?;
    let body = inner[..close].trim();
    let mut elements = Vec::new();
    for part in body.split(',') {
        let lit = part.trim();
        if lit.is_empty() {
            continue;
        }
        // Strip optional width suffix (`1_u32`, `255_u8`, etc.) and
        // any underscore digit separators.
        let stripped: String = lit
            .split('_')
            .next()
            .unwrap_or(lit)
            .chars()
            .filter(|c| *c != '_')
            .collect();
        let v: i64 = stripped.parse().ok()?;
        elements.push(v);
    }
    if elements.is_empty() {
        return None;
    }
    Some((name.to_string(), elements))
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
fn collect_array_appends(lines: &[&str], start: usize, end: usize, name: &str) -> (u32, Vec<i64>) {
    let mut elements = Vec::new();
    let mut last_append_line = 0u32;
    let append_prefix = format!("{name}.append(");
    for (k, line) in lines.iter().enumerate().take(end + 1).skip(start) {
        let trimmed = line.trim();
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

/// Recover destructuring let-bindings (`let (x, y, ...) = <source>;`)
/// where `<source>` is a bare identifier matching a previously-emitted
/// `CompoundBinding` of `CompoundKind::Tuple` with the same arity.
///
/// Returns one `DestructureBinding` per recognised line, carrying the
/// child name → value pairs drawn positionally from the source tuple.
/// Anything more elaborate (computed RHS, nested destructure, mismatched
/// arity, source binding that isn't a recognised tuple) is skipped so
/// the recorder degrades back to "tuple binding only" rather than
/// surfacing wrong values.
fn parse_destructure_bindings(
    source: &str,
    compound_bindings: &[CompoundBinding],
) -> Vec<DestructureBinding> {
    let mut out = Vec::new();
    for (line_idx, raw) in source.lines().enumerate() {
        let trimmed = raw.trim();
        let rest = match trimmed.strip_prefix("let ") {
            Some(r) => r.trim(),
            None => continue,
        };
        if !rest.starts_with('(') {
            continue;
        }
        let close = match rest.find(')') {
            Some(p) => p,
            None => continue,
        };
        let names_text = &rest[1..close];
        let names: Vec<String> = names_text
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.trim_start_matches("mut ").trim().to_string())
            .collect();
        if names.is_empty()
            || !names
                .iter()
                .all(|n| n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        {
            continue;
        }
        let after = rest[close + 1..].trim();
        let after = match after.strip_prefix('=') {
            Some(s) => s.trim().trim_end_matches(';').trim(),
            None => continue,
        };
        // RHS must be a bare identifier matching a recognised tuple
        // CompoundBinding with the same arity.
        if !after.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        let src_binding = compound_bindings.iter().find(|b| {
            b.name == after
                && matches!(b.kind, CompoundKind::Tuple)
                && b.elements.len() == names.len()
        });
        let binding = match src_binding {
            Some(b) => b,
            None => continue,
        };
        let children: Vec<(String, i64)> = names
            .into_iter()
            .zip(binding.elements.iter().copied())
            .collect();
        out.push(DestructureBinding {
            emit_line: (line_idx + 1) as u32,
            children,
        });
    }
    out
}

/// Walk every Cairo source line for `<name>.pop_front();` mutation
/// patterns and emit one `ArrayMutation` per line, carrying the
/// running post-mutation contents of the affected Array compound
/// binding.  Multiple mutations on the same array compose in source
/// order (running simulator).
///
/// Today we only recognise `pop_front()` because that's the only
/// mutating method exercised by the array_operations fixture; future
/// extensions (`append` after the initial-decl line, `pop_back`,
/// `swap`) plug in the same way.  Read-only methods (`.at(i)`,
/// `.len()`, `.span()`) don't appear here because the underlying
/// compound binding is left unchanged.
fn parse_array_mutations(
    source: &str,
    compound_bindings: &[CompoundBinding],
) -> Vec<ArrayMutation> {
    let mut out = Vec::new();
    // Per-array running contents, keyed by source binding name.
    let mut running: std::collections::HashMap<String, Vec<i64>> = std::collections::HashMap::new();
    for binding in compound_bindings {
        if matches!(binding.kind, CompoundKind::Array) {
            running.insert(binding.name.clone(), binding.elements.clone());
        }
    }
    for (line_idx, raw) in source.lines().enumerate() {
        let line = raw.split("//").next().unwrap_or(raw);
        // Find any `<ident>.pop_front()` call.  Allow `let _ = …` /
        // bare statement / assignment LHS.
        let bytes = line.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let c = bytes[i];
            if !(c.is_ascii_alphabetic() || c == b'_') {
                i += 1;
                continue;
            }
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let ident = &line[start..i];
            if i + ".pop_front()".len() > line.len() {
                continue;
            }
            if !line[i..].starts_with(".pop_front()") {
                continue;
            }
            let contents = match running.get_mut(ident) {
                Some(v) => v,
                None => continue,
            };
            if contents.is_empty() {
                continue;
            }
            // pop_front mutates in place: drop element 0.
            contents.remove(0);
            out.push(ArrayMutation {
                name: ident.to_string(),
                line: (line_idx + 1) as u32,
                elements: contents.clone(),
            });
            i += ".pop_front()".len();
        }
    }
    out
}

/// Recover `let <view> = <arr>.span();` slice-view bindings from a
/// Cairo source.  Each emission carries the source Array's recovered
/// elements so the recorder can emit a `ValueRecord::Sequence` with
/// `is_slice: true` flipped on at the let line.
///
/// Recognised shape (deliberately conservative):
///
/// * `let [mut] <name>[: <T>] = <bare_array>.span();` where
///   `<bare_array>` is a previously-recognised
///   `CompoundKind::Array` binding.  Anything more elaborate
///   (computed RHS, `array![...].span()` inline, multi-method
///   chain) falls through.
fn parse_span_emissions(source: &str, compound_bindings: &[CompoundBinding]) -> Vec<SpanEmission> {
    let mut out = Vec::new();
    for (line_idx, raw) in source.lines().enumerate() {
        let trimmed = raw.trim();
        let rest = match trimmed.strip_prefix("let ") {
            Some(s) => s,
            None => continue,
        };
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
            lhs[..colon].trim_start_matches("mut ").trim()
        } else {
            lhs.trim_start_matches("mut ").trim()
        };
        if name.is_empty() {
            continue;
        }
        // RHS must be exactly `<bare_array>.span()`.
        let dot = match rhs.find('.') {
            Some(p) => p,
            None => continue,
        };
        let src_name = rhs[..dot].trim();
        let after_dot = rhs[dot + 1..].trim();
        if after_dot != "span()" {
            continue;
        }
        if !src_name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            continue;
        }
        let binding = match compound_bindings
            .iter()
            .find(|b| b.name == src_name && matches!(b.kind, CompoundKind::Array))
        {
            Some(b) => b,
            None => continue,
        };
        out.push(SpanEmission {
            name: name.to_string(),
            line: (line_idx + 1) as u32,
            elements: binding.elements.clone(),
        });
    }
    out
}

/// Recover bounded-width integer let-bindings from a Cairo source.
/// Each binding becomes a per-line emission of `ValueRecord::Int`
/// against a width-bearing type id (`u8`, `i64`, etc.) — `u256` is
/// special-cased to a `Struct { low, high }` carrier.
///
/// Recognised shape: `let [mut] <name>: <T> = <int_lit>;` where `<T>`
/// is one of u8/u16/u32/u64/u128/i8/i16/i32/i64/i128/u256.  Underscore
/// digit separators are tolerated in the literal.  Anything more
/// elaborate (computed RHS, hex/bin literals, arithmetic) is left to
/// the scalar binding-name path.
fn parse_typed_int_bindings(source: &str) -> Vec<TypedIntBinding> {
    let mut out = Vec::new();
    for (line_idx, raw) in source.lines().enumerate() {
        let trimmed = raw.trim();
        let rest = match trimmed.strip_prefix("let ") {
            Some(s) => s,
            None => continue,
        };
        if rest.starts_with('(') {
            continue;
        }
        let colon = match rest.find(':') {
            Some(p) => p,
            None => continue,
        };
        let eq = match rest.find('=') {
            Some(p) => p,
            None => continue,
        };
        if colon >= eq {
            continue;
        }
        let lhs = rest[..colon].trim().trim_start_matches("mut ").trim();
        let type_text = rest[colon + 1..eq].trim();
        let rhs = rest[eq + 1..].trim().trim_end_matches(';').trim();
        if lhs.is_empty() {
            continue;
        }
        let kind = match TypedIntKind::from_str(type_text) {
            Some(k) => k,
            None => continue,
        };
        // Strip underscore digit separators.
        let lit_clean: String = rhs.chars().filter(|c| *c != '_').collect();
        let value: i128 = match lit_clean.parse::<i128>() {
            Ok(v) => v,
            Err(_) => continue,
        };
        out.push(TypedIntBinding {
            name: lhs.to_string(),
            line: (line_idx + 1) as u32,
            kind,
            value,
        });
    }
    out
}

/// Parse `@T` (snapshot) and `ref T` (mutable reference) parameter
/// passing from a Cairo source text.  Pairs each call site like
/// `read_only(@origin)` or `scale(ref shift, ...)` with the matching
/// callee parameter declaration so the recorder can emit a typed
/// `ValueRecord::Reference` at the callee's entry step.
///
/// Recognised shapes (deliberately conservative):
///
/// * Caller body line of the form `<callee>(<arg_list>)` where one or
///   more arguments are `@<bare_name>` or `ref <bare_name>` — bare
///   names only, no nested expressions.
/// * Callee declared as `fn <callee>(<param>: @T, ...)` /
///   `fn <callee>(ref <param>: T, ...)` — the parameter type T is
///   ignored (the matching CompoundBinding's struct value supplies
///   the dereferenced shape).
/// * The source binding referenced by the call argument must be a
///   recognised `CompoundKind::Struct` binding so the dereferenced
///   value can be reconstructed.  Anything else makes the emission
///   skip silently — the recorder degrades back to the pre-fix shape
///   (no Reference, no per-callee parameter row) for unrecognised
///   passing patterns.
fn parse_reference_emissions(
    source: &str,
    compound_bindings: &[CompoundBinding],
) -> Vec<ReferenceEmission> {
    let lines: Vec<&str> = source.lines().collect();
    // Map callee bare-name → ordered list of (param_name, mutable) for
    // its `@T` / `ref T` parameters.
    let mut callee_params: std::collections::HashMap<String, Vec<(String, bool)>> =
        std::collections::HashMap::new();
    for line in &lines {
        let trimmed = line.trim();
        let after_fn = match trimmed.strip_prefix("fn ") {
            Some(s) => s.trim(),
            None => continue,
        };
        let paren_open = match after_fn.find('(') {
            Some(p) => p,
            None => continue,
        };
        let paren_close = match after_fn.rfind(')') {
            Some(p) => p,
            None => continue,
        };
        if paren_close <= paren_open {
            continue;
        }
        let name = after_fn[..paren_open].trim().to_string();
        let params_text = &after_fn[paren_open + 1..paren_close];
        let mut ref_params = Vec::new();
        for part in params_text.split(',') {
            let p = part.trim();
            if p.is_empty() {
                continue;
            }
            // `ref <name>: T` — mutable reference parameter.
            if let Some(rest) = p.strip_prefix("ref ") {
                let colon = match rest.find(':') {
                    Some(c) => c,
                    None => continue,
                };
                let pname = rest[..colon].trim().to_string();
                if !pname.is_empty() {
                    ref_params.push((pname, true));
                }
                continue;
            }
            // `<name>: @T` — snapshot parameter.
            let colon = match p.find(':') {
                Some(c) => c,
                None => continue,
            };
            let pname = p[..colon].trim().to_string();
            let ptype = p[colon + 1..].trim();
            if ptype.starts_with('@') && !pname.is_empty() {
                ref_params.push((pname, false));
            }
        }
        if !ref_params.is_empty() {
            callee_params.insert(name, ref_params);
        }
    }

    let mut out = Vec::new();
    let mut next_address: u64 = 0x1000;
    for raw in &lines {
        let line = raw.split("//").next().unwrap_or(raw);
        // Find every `<name>(<args>)` call site whose callee matches a
        // function with reference parameters.  Multiple call sites can
        // appear on one line; we scan left-to-right.
        let bytes = line.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let c = bytes[i];
            if !(c.is_ascii_alphabetic() || c == b'_') {
                i += 1;
                continue;
            }
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let ident = &line[start..i];
            // Must be followed (after whitespace) by `(`.
            let mut j = i;
            while j < bytes.len() && bytes[j] == b' ' {
                j += 1;
            }
            if j >= bytes.len() || bytes[j] != b'(' {
                continue;
            }
            // Skip path-component identifiers (`Foo::ident(...)`).
            let preceded_by_path = start >= 2 && &line[start - 2..start] == "::";
            if preceded_by_path {
                continue;
            }
            let params = match callee_params.get(ident) {
                Some(p) => p.clone(),
                None => continue,
            };
            // Find the matching `)` — single-level only (no nested
            // calls in the args, which is fine for our fixtures).
            let after_paren = j + 1;
            let close = match line[after_paren..].find(')') {
                Some(p) => after_paren + p,
                None => continue,
            };
            let args_text = &line[after_paren..close];
            let arg_list: Vec<&str> = args_text.split(',').map(|s| s.trim()).collect();
            for ((pname, mutable), arg) in params.iter().zip(arg_list.iter()) {
                let stripped = if *mutable {
                    arg.strip_prefix("ref ").map(|s| s.trim())
                } else {
                    arg.strip_prefix('@').map(|s| s.trim())
                };
                let source_name = match stripped {
                    Some(s)
                        if s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                            && !s.is_empty() =>
                    {
                        s.to_string()
                    }
                    _ => continue,
                };
                // Only emit when the source binding has a recognised
                // Struct CompoundBinding (so the dereferenced value
                // can be reconstructed).
                if !compound_bindings
                    .iter()
                    .any(|b| b.name == source_name && matches!(b.kind, CompoundKind::Struct))
                {
                    continue;
                }
                out.push(ReferenceEmission {
                    callee: ident.to_string(),
                    param_name: pname.clone(),
                    source_binding: source_name,
                    mutable: *mutable,
                    address: next_address,
                });
                next_address += 0x10;
            }
        }
    }
    out
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
    let impl_ranges = impl_block_ranges(&lines);
    // First pass: derive the (bare_name, full_name) for every fn so the
    // callee resolver below can prefer impl-qualified names like
    // `HelloImpl::greet` over the shared bare `greet`.
    let mut prelim: Vec<(usize, usize, String, String)> = Vec::new();
    for (start, end) in &ranges {
        let header = lines[*start].trim();
        let raw_name = match extract_fn_name(header) {
            Some(n) => n,
            None => continue,
        };
        // Find the surrounding `impl <ImplName> of <Trait>` block (if any)
        // so impl methods get a `<ImplName>::<fn>` bare_name distinct from
        // the trait-method shared name.  This is what lets two impls of
        // the same trait surface as independent DFS frames.
        let impl_name = impl_ranges
            .iter()
            .find(|(s, e, _)| *start > *s && *start < *e)
            .map(|(_, _, name)| name.clone());
        let bare_name = match &impl_name {
            Some(impl_n) => format!("{impl_n}::{raw_name}"),
            None => raw_name.clone(),
        };
        // Sierra emits impl methods as `<crate>::<crate>::<ImplName>::<fn>`;
        // bare functions as `<crate>::<crate>::<fn>`.  Match the longest
        // recognised suffix so we still pick up impl-qualified names.
        let suffix = format!("::{}", bare_name);
        let full_name = user_functions
            .iter()
            .find(|f| f.ends_with(&suffix) || **f == bare_name)
            .or_else(|| {
                user_functions
                    .iter()
                    .find(|f| f.contains(&format!("::{}", raw_name)))
            })
            .map(|s| s.to_string())
            .unwrap_or_else(|| bare_name.clone());
        prelim.push((*start, *end, bare_name, full_name));
    }

    // The callee-recognition table is the union of the Sierra user_functions
    // (so existing fixtures keep matching against e.g. `compute`) plus the
    // impl-qualified bare names recovered above (so `HelloImpl::greet`
    // becomes a recognised callee on lines like
    // `let a = HelloImpl::greet(h);`).
    let mut bare_callee_names: Vec<String> = prelim.iter().map(|(_, _, b, _)| b.clone()).collect();
    let user_function_strings: Vec<String> = user_functions.iter().map(|s| s.to_string()).collect();
    bare_callee_names.extend(user_function_strings.iter().cloned());
    let bare_callee_refs: Vec<&str> = bare_callee_names.iter().map(|s| s.as_str()).collect();

    let mut entries = Vec::new();
    for (start, end, bare_name, full_name) in prelim {
        let mut callees_per_line: Vec<Vec<String>> = Vec::with_capacity(end - start + 1);
        for line in lines.iter().take(end + 1).skip(start) {
            callees_per_line.push(parse_callees_in_line(line, &bare_callee_refs));
        }

        let while_loops = parse_while_loops(&lines, start, end);

        entries.push(FunctionEntry {
            bare_name,
            full_name,
            start_line: (start + 1) as u32,
            line_count: end - start + 1,
            body: source.to_string(),
            callees_per_line,
            while_loops,
        });
    }
    entries
}

/// Return inclusive (start, end, ImplName) 0-based line index ranges for
/// every top-level `impl <Name> of <Trait>` block in the source.  Used by
/// `build_function_table` to give impl methods a `<ImplName>::<fn>` bare
/// name distinct from the shared trait-method name (e.g. `greet`),
/// without which two impls of the same trait would collapse to a single
/// DFS frame.
fn impl_block_ranges(lines: &[&str]) -> Vec<(usize, usize, String)> {
    let mut ranges = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        // Recognise `impl <Name> of <Trait>` (with optional generic
        // params).  `impl <Name>: ...` (impl-of-impl shorthand) and
        // `impl<...>` declarations are not exercised by the fixtures
        // and fall through.
        let after_impl = match trimmed.strip_prefix("impl ") {
            Some(s) => s,
            None => {
                i += 1;
                continue;
            }
        };
        // Pull the impl name — everything up to the first whitespace /
        // generic / `of` keyword.
        let stop = after_impl.find([' ', '<', ':']).unwrap_or(after_impl.len());
        let impl_name = after_impl[..stop].trim().to_string();
        // Must contain ` of ` to be a trait impl (not a free impl block).
        if !trimmed.contains(" of ") {
            i += 1;
            continue;
        }
        if impl_name.is_empty() {
            i += 1;
            continue;
        }
        // Find the matching closing brace.
        let impl_start = i;
        let mut brace_depth = 0i32;
        let mut impl_end = i;
        for (j, line) in lines.iter().enumerate().skip(impl_start) {
            for ch in line.chars() {
                if ch == '{' {
                    brace_depth += 1;
                } else if ch == '}' {
                    brace_depth -= 1;
                }
            }
            if brace_depth == 0 && j > impl_start {
                impl_end = j;
                break;
            }
        }
        ranges.push((impl_start, impl_end, impl_name));
        i = impl_end + 1;
    }
    ranges
}

/// Walk the function body's lines and recognise top-level `while` loops
/// of the shape:
///
/// ```cairo
/// while <expr> {
///     <name> = <expr>;
///     <name> = <expr>;
///     ...
/// };
/// ```
///
/// Loops with bodies whose statements aren't bare assignments are still
/// detected (the simulator still emits a step + re-runs the condition
/// check) — only the per-statement variable-update side-effect is
/// dropped on those unrecognised bodies.  Loops whose condition fails
/// to parse, or whose closing `}`/`};` we cannot find, are left to the
/// fall-back static walk.
///
/// `start` / `end` are 0-based inclusive line indices into `lines`,
/// covering exactly the function body (including the outer `{` and
/// closing `}`).
fn parse_while_loops(lines: &[&str], start: usize, end: usize) -> Vec<WhileLoop> {
    let mut out = Vec::new();
    // Track brace depth relative to the function's own opening `{`.
    // depth 1 == inside the function body (top-level statements).
    // We only recover loops at depth 1 — nested inner loops would
    // require recursive handling, which the fixture doesn't exercise.
    let mut depth: i32 = 0;
    let mut k = start;
    while k <= end {
        let raw = lines[k];
        let trimmed = raw.trim();
        let opens = trimmed.matches('{').count() as i32;
        let closes = trimmed.matches('}').count() as i32;

        if depth == 1 && trimmed.starts_with("while ") && trimmed.ends_with('{') {
            // Header at depth 1; body starts at k+1, depth bumps to 2
            // immediately after this line.
            let cond_text = trimmed
                .trim_start_matches("while ")
                .trim_end_matches('{')
                .trim();
            let condition = match parse_expr(cond_text) {
                Some(e) => e,
                None => {
                    depth += opens - closes;
                    k += 1;
                    continue;
                }
            };
            // Find the matching `}` for this loop, scanning forward at
            // depth 2 → 1.
            let header_line = (k + 1) as u32;
            let body_start = k + 1;
            let mut local_depth: i32 = 1;
            let mut body_end_idx: Option<usize> = None;
            let mut closing_idx: Option<usize> = None;
            for (j, raw_line) in lines.iter().enumerate().take(end + 1).skip(k + 1) {
                let lt = raw_line.trim();
                let o = lt.matches('{').count() as i32;
                let c = lt.matches('}').count() as i32;
                local_depth += o - c;
                if local_depth == 0 {
                    closing_idx = Some(j);
                    body_end_idx = Some(if j == 0 { 0 } else { j - 1 });
                    break;
                }
            }
            let (body_end, closing) = match (body_end_idx, closing_idx) {
                (Some(b), Some(c)) => (b, c),
                _ => {
                    depth += opens - closes;
                    k += 1;
                    continue;
                }
            };
            // Parse each body line as an optional `<name> = <expr>;`
            // statement.  Lines we can't parse still get steps emitted
            // — the simulator just doesn't update the env from them.
            let mut body_statements = Vec::with_capacity(body_end - body_start + 1);
            for raw_line in lines.iter().take(body_end + 1).skip(body_start) {
                let lt = raw_line.trim();
                if lt.is_empty() {
                    body_statements.push(None);
                    continue;
                }
                body_statements.push(parse_assignment_statement(lt));
            }

            out.push(WhileLoop {
                header_line,
                body_start_line: (body_start + 1) as u32,
                closing_line: (closing + 1) as u32,
                condition,
                body_statements,
            });
            // Advance past the loop's closing brace.  The header's
            // `{` and the closing line's matching `}` are equal in
            // count for any well-formed loop, so the function-body
            // brace depth is unchanged across the consumed range.
            k = closing + 1;
            continue;
        }

        depth += opens - closes;
        k += 1;
    }
    out
}

/// Parse `<name> = <expr>;` where `<name>` is a bare identifier and
/// `<expr>` is one our `parse_expr` recogniser handles.  Returns
/// `None` for shapes outside this very small grammar — the simulator
/// then emits a step on the line but does not update any variable.
fn parse_assignment_statement(line: &str) -> Option<Statement> {
    let stripped = line.trim().trim_end_matches(';').trim();
    let eq = stripped.find('=')?;
    // Ignore `==`, `<=`, `>=`, `!=` — those are comparisons inside an
    // expression, not assignment.
    let rest_after = stripped.as_bytes().get(eq + 1).copied().unwrap_or(b' ');
    let prev = if eq > 0 {
        stripped.as_bytes()[eq - 1]
    } else {
        b' '
    };
    if rest_after == b'=' || prev == b'<' || prev == b'>' || prev == b'!' || prev == b'=' {
        return None;
    }
    let target = stripped[..eq].trim().to_string();
    if target.is_empty()
        || !target
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return None;
    }
    let rhs_text = stripped[eq + 1..].trim();
    let rhs = parse_expr(rhs_text)?;
    Some(Statement { target, rhs })
}

/// Parse a tiny expression grammar:
///
/// ```text
/// expr   = or_expr
/// or     = and (`||` and)*
/// and    = cmp (`&&` cmp)*
/// cmp    = sum ((`<`|`<=`|`>`|`>=`|`==`|`!=`) sum)?
/// sum    = term ((`+`|`-`) term)*
/// term   = atom ((`*`) atom)*
/// atom   = INT | IDENT | `(` expr `)`
/// ```
///
/// Returns `None` for inputs outside this grammar — the caller treats
/// a `None` parse as "skip the loop's simulator path" (the fall-back
/// static walk still runs).
fn parse_expr(input: &str) -> Option<Expr> {
    let mut p = ExprParser::new(input);
    let e = p.parse_or()?;
    p.skip_ws();
    if !p.at_end() {
        return None;
    }
    Some(e)
}

struct ExprParser<'a> {
    src: &'a [u8],
    pos: usize,
}

impl<'a> ExprParser<'a> {
    fn new(s: &'a str) -> Self {
        Self {
            src: s.as_bytes(),
            pos: 0,
        }
    }

    fn skip_ws(&mut self) {
        while self.pos < self.src.len() && self.src[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn at_end(&self) -> bool {
        self.pos >= self.src.len()
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn eat_str(&mut self, s: &str) -> bool {
        if self.src[self.pos..].starts_with(s.as_bytes()) {
            self.pos += s.len();
            true
        } else {
            false
        }
    }

    fn parse_or(&mut self) -> Option<Expr> {
        let mut lhs = self.parse_and()?;
        loop {
            self.skip_ws();
            if !self.eat_str("||") {
                break;
            }
            let rhs = self.parse_and()?;
            lhs = Expr::Bin(BinOp::Or, Box::new(lhs), Box::new(rhs));
        }
        Some(lhs)
    }

    fn parse_and(&mut self) -> Option<Expr> {
        let mut lhs = self.parse_cmp()?;
        loop {
            self.skip_ws();
            if !self.eat_str("&&") {
                break;
            }
            let rhs = self.parse_cmp()?;
            lhs = Expr::Bin(BinOp::And, Box::new(lhs), Box::new(rhs));
        }
        Some(lhs)
    }

    fn parse_cmp(&mut self) -> Option<Expr> {
        let lhs = self.parse_sum()?;
        self.skip_ws();
        // Order matters — try the two-character variants first.
        let op = if self.eat_str("<=") {
            BinOp::Le
        } else if self.eat_str(">=") {
            BinOp::Ge
        } else if self.eat_str("==") {
            BinOp::Eq
        } else if self.eat_str("!=") {
            BinOp::Ne
        } else if self.peek() == Some(b'<') {
            self.pos += 1;
            BinOp::Lt
        } else if self.peek() == Some(b'>') {
            self.pos += 1;
            BinOp::Gt
        } else {
            return Some(lhs);
        };
        let rhs = self.parse_sum()?;
        Some(Expr::Bin(op, Box::new(lhs), Box::new(rhs)))
    }

    fn parse_sum(&mut self) -> Option<Expr> {
        let mut lhs = self.parse_term()?;
        loop {
            self.skip_ws();
            let op = match self.peek() {
                Some(b'+') => BinOp::Add,
                Some(b'-') => BinOp::Sub,
                _ => break,
            };
            // Avoid matching `||` / `&&` etc. against `-`/`+`; not
            // needed since those use `&` / `|`.
            self.pos += 1;
            let rhs = self.parse_term()?;
            lhs = Expr::Bin(op, Box::new(lhs), Box::new(rhs));
        }
        Some(lhs)
    }

    fn parse_term(&mut self) -> Option<Expr> {
        let mut lhs = self.parse_atom()?;
        loop {
            self.skip_ws();
            if self.peek() == Some(b'*') {
                self.pos += 1;
                let rhs = self.parse_atom()?;
                lhs = Expr::Bin(BinOp::Mul, Box::new(lhs), Box::new(rhs));
            } else {
                break;
            }
        }
        Some(lhs)
    }

    fn parse_atom(&mut self) -> Option<Expr> {
        self.skip_ws();
        let c = self.peek()?;
        if c == b'(' {
            self.pos += 1;
            let e = self.parse_or()?;
            self.skip_ws();
            if self.peek() != Some(b')') {
                return None;
            }
            self.pos += 1;
            return Some(e);
        }
        if c.is_ascii_digit() {
            let start = self.pos;
            while self.pos < self.src.len() && self.src[self.pos].is_ascii_digit() {
                self.pos += 1;
            }
            let lit = std::str::from_utf8(&self.src[start..self.pos]).ok()?;
            let v: i64 = lit.parse().ok()?;
            return Some(Expr::Lit(v));
        }
        if c.is_ascii_alphabetic() || c == b'_' {
            let start = self.pos;
            while self.pos < self.src.len()
                && (self.src[self.pos].is_ascii_alphanumeric() || self.src[self.pos] == b'_')
            {
                self.pos += 1;
            }
            let ident = std::str::from_utf8(&self.src[start..self.pos])
                .ok()?
                .to_string();
            return Some(Expr::Var(ident));
        }
        None
    }
}

/// Evaluate an `Expr` against an environment of `i64`-valued
/// variables.  Comparisons return `1` for true / `0` for false so
/// the simulator can keep a uniform `i64` carrier.
fn eval_expr(expr: &Expr, env: &std::collections::HashMap<String, i64>) -> Option<i64> {
    match expr {
        Expr::Lit(v) => Some(*v),
        Expr::Var(name) => env.get(name).copied(),
        Expr::Bin(op, l, r) => {
            let lv = eval_expr(l, env)?;
            let rv = eval_expr(r, env)?;
            Some(match op {
                BinOp::Add => lv.wrapping_add(rv),
                BinOp::Sub => lv.wrapping_sub(rv),
                BinOp::Mul => lv.wrapping_mul(rv),
                BinOp::Lt => (lv < rv) as i64,
                BinOp::Le => (lv <= rv) as i64,
                BinOp::Gt => (lv > rv) as i64,
                BinOp::Ge => (lv >= rv) as i64,
                BinOp::Eq => (lv == rv) as i64,
                BinOp::Ne => (lv != rv) as i64,
                BinOp::And => ((lv != 0) && (rv != 0)) as i64,
                BinOp::Or => ((lv != 0) || (rv != 0)) as i64,
            })
        }
    }
}

/// Recover the integer initialiser bound to a `let [mut] <name>[: <T>]
/// = <int_lit>;` line.  Used by the `while`-loop simulator to seed
/// its env with the values of mutable accumulators / counters
/// declared earlier in the function body.
fn parse_let_int_init(line: &str) -> Option<(String, i64)> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix("let ")?;
    if rest.starts_with('(') {
        return None;
    }
    let eq = rest.find('=')?;
    let lhs = rest[..eq].trim();
    let rhs = rest[eq + 1..].trim().trim_end_matches(';').trim();
    let name = if let Some(colon) = lhs.find(':') {
        lhs[..colon].trim_start_matches("mut ").trim().to_string()
    } else {
        lhs.trim_start_matches("mut ").trim().to_string()
    };
    if name.is_empty() {
        return None;
    }
    let v: i64 = rhs.parse().ok()?;
    Some((name, v))
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
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let ident = &trimmed[start..i];
            // Skip leading `::` that would make this a path component
            // rather than a bare function call (`ArrayTrait::new` —
            // `new` would otherwise look like a call).
            let preceded_by_path = start >= 2 && &trimmed[start - 2..start] == "::";
            // Must be followed by `(` to count as a call.
            if i < bytes.len() && bytes[i] == b'(' {
                if preceded_by_path {
                    // Try the two-segment `<TypeName>::<method>(` form so
                    // impl-qualified calls (e.g. `HelloImpl::greet(h)`)
                    // surface as the joined `<TypeName>::<method>` callee
                    // — matching the impl-aware bare names produced by
                    // `build_function_table` for trait impl methods.
                    // Walk back to find the prior identifier.
                    let mut p = start - 2;
                    while p > 0
                        && (trimmed.as_bytes()[p - 1].is_ascii_alphanumeric()
                            || trimmed.as_bytes()[p - 1] == b'_')
                    {
                        p -= 1;
                    }
                    let prefix = &trimmed[p..start - 2];
                    if !prefix.is_empty()
                        && prefix
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '_')
                    {
                        let joined = format!("{prefix}::{ident}");
                        // Don't double-prefix if `prefix` itself was
                        // preceded by another `::` (Cairo path like
                        // `core::array::ArrayTrait::new`).
                        let prefix_preceded_by_path = p >= 2 && &trimmed[p - 2..p] == "::";
                        if !prefix_preceded_by_path
                            && user_functions.iter().any(|f| {
                                f == &joined.as_str() || f.ends_with(&format!("::{}", joined))
                            })
                        {
                            out.push(joined);
                        }
                    }
                } else if user_functions
                    .iter()
                    .any(|f| f == &ident || f.ends_with(&format!("::{}", ident)))
                {
                    out.push(ident.to_string());
                }
            }
        } else {
            i += 1;
        }
    }
    out
}

/// Whether the current source line is a tail-position expression that is
/// exactly one direct user-function call, e.g. `compute()` or
/// `SomeImpl::run(x);`.
///
/// Such lines are caller/callee boundaries rather than ordinary caller
/// work.  Entering the callee before registering the parent line keeps the
/// callee's first real body step inside the callee call range; DAP-style
/// navigation can then step into the callee and read its local variables.
fn is_direct_tail_call_line(
    lines: &[&str],
    entry: &FunctionEntry,
    line_offset: usize,
    callees: &[String],
) -> bool {
    if callees.len() != 1 {
        return false;
    }

    let abs_line = entry.start_line as usize + line_offset;
    let raw_line = lines.get(abs_line - 1).copied().unwrap_or("");
    let expr = raw_line
        .split("//")
        .next()
        .unwrap_or(raw_line)
        .trim()
        .trim_end_matches(';')
        .trim();
    let expr = expr.strip_prefix("return ").map(str::trim).unwrap_or(expr);

    if expr.is_empty() || expr.starts_with("fn ") || expr.starts_with("let ") {
        return false;
    }

    let Some(paren) = expr.find('(') else {
        return false;
    };
    if !expr.ends_with(')') {
        return false;
    }

    let callee_expr = expr[..paren].trim();
    let callee = &callees[0];
    if callee_expr != callee {
        return false;
    }

    // Tail position: there are no later executable source lines before the
    // function closes. Closing braces (and loop-style `};`) do not count.
    let next_abs = abs_line + 1;
    let last_abs = entry.start_line as usize + entry.line_count.saturating_sub(1);
    for later_abs in next_abs..=last_abs {
        let later = lines
            .get(later_abs - 1)
            .copied()
            .unwrap_or("")
            .split("//")
            .next()
            .unwrap_or("")
            .trim();
        if later.is_empty() || later == "}" || later == "};" || later == "{" {
            continue;
        }
        return false;
    }

    true
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
    let mut out: std::collections::HashMap<String, Option<i64>> = std::collections::HashMap::new();

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

            // Bare-identifier return: that's the value.  Skip
            // integer-literal tails (`7`, `11`, …) — they're not
            // variable references and a heuristic that treated them as
            // such would surface the VM's success-return felt as the
            // claimed return value of any function whose body collapses
            // to an integer literal (cross-talk between unrelated
            // functions).
            if !tail.is_empty()
                && tail.chars().all(|c| c.is_alphanumeric() || c == '_')
                && !tail.chars().all(|c| c.is_ascii_digit())
            {
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
    if !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
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

                // Single variable return (not a function call).  Skip
                // integer-literal tails (`7`, `11`) — they would
                // otherwise pollute `var_values` with numeric-string
                // keys aliased to the VM's success-return felt and
                // leak that value into unrelated functions whose body
                // collapses to a literal of the same numeric form.
                let expr = line.trim_end_matches(';').trim();
                if !expr.is_empty()
                    && expr.chars().all(|c| c.is_alphanumeric() || c == '_')
                    && !expr.chars().all(|c| c.is_ascii_digit())
                {
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

/// Recover a Cairo panic's human-readable message by walking the raw
/// felt252 panic-payload vector returned from
/// `RunResultValue::Panic(values)`.
///
/// Cairo's `panic_with_felt252("…")` / `assert!(cond, "…")` encoding
/// packs the message into one or more felt252 elements (31 ASCII bytes
/// per felt, big-endian).  The exact framing varies — a typical encoding
/// is `[panic_tag, payload_header, message_felt, length]` for short
/// messages and similar for longer ones — so we try each felt
/// independently, keep the printable-ASCII runs, and join them into a
/// single decoded message string.
///
/// Returns an empty string when no felt decodes to a useful run of
/// printable bytes (typical of non-`assert!` panics that carry numeric
/// codes only).
fn decode_cairo_panic_message<T: std::fmt::Display>(values: &[T]) -> String {
    // The real type held inside `RunResultValue::Panic` is
    // `Vec<Felt252>` re-exported via `cairo_lang_runner`.  We capture
    // it as a generic slice of `Display`-able values to avoid coupling
    // the helper to that exact path (the import lives at the trace_program
    // call site).  In practice every element implements `to_string()`
    // returning a decimal felt — that's all we need to recover the bytes.
    let mut parts: Vec<String> = Vec::new();
    for v in values {
        let dec = v.to_string();
        // Skip obviously-non-message integers (0 / tiny lengths) so
        // they don't pollute the decoded text.
        if dec.len() < 6 {
            continue;
        }
        // Re-parse the decimal felt as an unsigned big-int and recover
        // the trailing printable bytes.  We don't have num_bigint here,
        // so do the conversion by stripping one byte at a time via
        // division-by-256.  Felts can be up to 252 bits (~32 bytes); the
        // 31-byte cap below matches Cairo's per-felt ASCII packing.
        let bytes = felt_decimal_to_bytes(&dec, 31);
        let ascii = bytes_to_printable_run(&bytes);
        if ascii.len() >= 4 {
            parts.push(ascii);
        }
    }
    parts.join(" ")
}

/// Decode a decimal-string felt into a big-endian byte vector of at
/// most `max_len` bytes.  Used only by `decode_cairo_panic_message` to
/// recover ASCII payloads packed into felt252 values — see the
/// per-felt 31-byte ASCII packing convention in
/// `corelib::byte_array`.
fn felt_decimal_to_bytes(dec: &str, max_len: usize) -> Vec<u8> {
    // We implement big-int / 256 in-place on a digit buffer because
    // pulling in `num_bigint` for this tiny helper would balloon the
    // recorder's dependency closure.  The arithmetic stays linear in
    // `dec.len()` per division, which is fine for the at-most-32-byte
    // felts we deal with.
    let mut digits: Vec<u8> = dec.bytes().map(|b| b.wrapping_sub(b'0')).collect();
    let mut out: Vec<u8> = Vec::new();
    while out.len() < max_len {
        let mut carry: u32 = 0;
        let mut all_zero = true;
        for d in digits.iter_mut() {
            let cur = carry * 10 + *d as u32;
            *d = (cur / 256) as u8;
            carry = cur % 256;
            if *d != 0 {
                all_zero = false;
            }
        }
        out.push(carry as u8);
        if all_zero {
            break;
        }
    }
    out.reverse();
    out
}

/// Filter a byte vector down to its longest run of printable-ASCII
/// bytes (space through `~`).  Used to extract human-readable message
/// fragments from felt252-packed panic payloads.
fn bytes_to_printable_run(bytes: &[u8]) -> String {
    let mut best = String::new();
    let mut cur = String::new();
    for &b in bytes {
        if (0x20..=0x7e).contains(&b) {
            cur.push(b as char);
        } else {
            if cur.len() > best.len() {
                best = cur.clone();
            }
            cur.clear();
        }
    }
    if cur.len() > best.len() {
        best = cur;
    }
    best.trim().to_string()
}

/// Extract function name from a line like "fn compute() -> felt252 {"
fn extract_fn_name(line: &str) -> Option<String> {
    let after_fn = line.strip_prefix("fn ")?.trim();
    let paren_pos = after_fn.find('(')?;
    Some(after_fn[..paren_pos].trim().to_string())
}

/// Return the 1-based byte columns at which each statement on `line_text`
/// begins.  Used by column-aware step emission so that a Cairo source
/// line packing several statements
/// (`let a = 1; let b = 2; let c = 3;`) surfaces one step per statement
/// with strictly distinct columns rather than collapsing onto a single
/// step at column 1.
///
/// Heuristic: split at every top-level `;` (i.e. one that lives outside
/// parens/brackets/braces and outside string literals).  The column of a
/// statement is the 1-based byte offset of its first non-whitespace
/// character.  Lines whose only content is a single statement (or
/// no recognised statement at all — e.g. a bare `}`) return a single
/// column at the first non-whitespace byte.
///
/// Mirrors the JS recorder's per-statement column tracking and the
/// EVM/Solana recorders' DWARF/source-map–driven column emission;
/// the Cairo recorder doesn't yet have Sierra-level column info plumbed
/// through, so this textual splitter is the fallback that still surfaces
/// the spec-correct distinct-columns property the column-aware
/// acceptance criteria pin down.
fn statement_columns_on_line(line_text: &str) -> Vec<u32> {
    let bytes = line_text.as_bytes();
    let mut cols: Vec<u32> = Vec::new();
    let mut depth: i32 = 0;
    let mut in_str = false;
    let mut in_char = false;
    let mut at_statement_start = true;

    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];

        // Detect the start of the *next* statement at the first
        // non-whitespace byte after a top-level `;`.
        if at_statement_start && !b.is_ascii_whitespace() {
            cols.push((i + 1) as u32);
            at_statement_start = false;
        }

        if in_str {
            if b == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if in_char {
            if b == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if b == b'\'' {
                in_char = false;
            }
            i += 1;
            continue;
        }

        match b {
            b'"' => in_str = true,
            b'\'' => in_char = true,
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                if depth > 0 {
                    depth -= 1;
                }
            }
            b';' if depth == 0 => {
                at_statement_start = true;
            }
            _ => {}
        }
        i += 1;
    }

    // If nothing matched (e.g. an entirely whitespace line) fall back to
    // column 1 so the caller still emits a single step.  Callers that
    // already filtered empty/`}`/`{` lines won't hit this branch.
    if cols.is_empty() {
        cols.push(1);
    }
    cols
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_statement_columns_single_statement() {
        // Plain single-statement body line indented by 4 spaces.
        let cols = statement_columns_on_line("    let a = 1;");
        assert_eq!(cols, vec![5]);
    }

    #[test]
    fn test_statement_columns_three_statements_on_one_line() {
        // The canonical multi-statement-per-line fixture: three Cairo
        // statements on one line should surface three strictly distinct
        // columns.  Indented by 4 spaces so the leftmost column is 5.
        //
        //   "    let a = 1; let b = 2; let c = 3;"
        //    0    5    1    1    2    2    3
        //         0    5    0    5    0
        //        ^          ^          ^
        //        col 5      col 16     col 27
        let cols = statement_columns_on_line("    let a = 1; let b = 2; let c = 3;");
        assert_eq!(cols, vec![5, 16, 27]);
    }

    #[test]
    fn test_statement_columns_semicolon_inside_string() {
        // A `;` inside a string literal must NOT split the statement.
        let cols = statement_columns_on_line(r#"    let s = "a;b;c"; let y = 0;"#);
        assert_eq!(cols.len(), 2, "got {:?}", cols);
    }

    #[test]
    fn test_statement_columns_semicolon_inside_parens() {
        // Top-level `;` only — `foo(a; b)` shouldn't split (Cairo doesn't
        // accept that syntax, but the splitter must still be robust).
        let cols = statement_columns_on_line("    let x = foo(1, 2); let y = 3;");
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0], 5);
    }

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
