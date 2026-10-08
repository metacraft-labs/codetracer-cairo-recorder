//! Runtime values of named Cairo locals, read from the executed program.
//!
//! The optimised build that produces the program's result folds and
//! inlines locals away, so it cannot tell which runtime value belongs to
//! which source variable.  This module compiles the same source a second
//! time with optimisations disabled (no inlining, no constant folding)
//! and with the compiler's variable debug info, runs it on the Cairo VM
//! with the execution trace enabled, and reads each Sierra variable's
//! value out of VM memory at the point where the program consumes it.
//!
//! Sierra variables are named through the compiler's function debug info
//! (Sierra variable -> Cairo variable name and declaration span).  A
//! `let x = y;` binding does not produce a Sierra variable of its own:
//! the compiler binds `x` to the very same value as `y`.  Such copy
//! bindings are read from the compiler's semantic model, and the value
//! observed for one side is attributed to the other.
//!
//! Every value reported here was read from the memory of the executed
//! program; nothing is derived from the source text.

use std::collections::HashMap;
use std::path::Path;

use cairo_lang_casm::cell_expression::{CellExpression, CellOperator};
use cairo_lang_casm::operand::{CellRef, DerefOrImmediate, Register};
use cairo_lang_compiler::db::RootDatabase;
use cairo_lang_compiler::project::setup_project;
use cairo_lang_compiler::CompilerConfig;
use cairo_lang_defs::diagnostic_utils::StableLocation;
use cairo_lang_defs::ids::{LanguageElementId, VarId as SemanticVarId};
use cairo_lang_filesystem::db::{init_dev_corelib, FilesGroup};
use cairo_lang_filesystem::ids::CrateInput;
use cairo_lang_lowering::optimizations::config::Optimizations;
use cairo_lang_runnable_utils::builder::RunnableBuilder;
use cairo_lang_runner::casm_run::run_function;
use cairo_lang_runner::{initialize_vm, SierraCasmRunner};
use cairo_lang_semantic::items::function_with_body::FunctionWithBodySemantic;
use cairo_lang_semantic::{Expr, Pattern, Statement};
use cairo_lang_sierra::debug_info::Annotations;
use cairo_lang_sierra::program::{GenericArg, Program, Statement as SierraStatement};
use cairo_lang_sierra_generator::db::SierraGenGroup;
use cairo_lang_sierra_to_casm::compiler::StatementKindDebugInfo;
use cairo_lang_utils::ordered_hash_map::OrderedHashMap;
use cairo_vm::Felt252;
use num_bigint::{BigInt, BigUint};

/// A Cairo local's declaration site: 0-based line and column of the
/// declaring identifier in the recorded source file.
type DeclPos = (usize, usize);

/// The runtime value of every named Cairo local of the recorded source
/// file whose value the executed program observably held.
#[derive(Debug, Default)]
pub struct ObservedLocals {
    /// `(name, 1-based declaration line)` -> value.
    values: HashMap<(String, u32), i64>,
}

impl ObservedLocals {
    /// The value of the local `name` declared on the 1-based `line`.
    pub fn get(&self, name: &str, line: u32) -> Option<i64> {
        self.values.get(&(name.to_string(), line)).copied()
    }

    /// Number of locals with an observed value.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether no local value was observed.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// Observe the runtime values of the named locals of `source_path`.
///
/// Returns an error when the unoptimised build cannot be compiled or
/// run; callers treat that as "no values observed".
pub fn observe_locals(source_path: &Path, corelib_path: &Path) -> eyre::Result<ObservedLocals> {
    let mut db = RootDatabase::builder()
        .with_optimizations(Optimizations::Disabled)
        .build()
        .map_err(|e| eyre::eyre!("failed to build database: {e}"))?;
    init_dev_corelib(&mut db, corelib_path.to_path_buf());
    let main_crate_ids = setup_project(&mut db, source_path)
        .map_err(|e| eyre::eyre!("failed to setup project: {e}"))?;
    let compiled = cairo_lang_compiler::compile_prepared_db(
        &db,
        CrateInput::into_crate_ids(&db, main_crate_ids),
        CompilerConfig {
            replace_ids: true,
            ..CompilerConfig::default()
        },
    )
    .map_err(|e| eyre::eyre!("unoptimised compilation failed: {e}"))?;
    let program = compiled.program.clone();

    let source_file = source_path
        .canonicalize()
        .unwrap_or_else(|_| source_path.to_path_buf());

    // Sierra function id -> Sierra var id -> Cairo local declaration.
    let debug_names = sierra_variable_names(
        &Annotations::from(
            compiled
                .debug_info
                .functions_info
                .extract_serializable_debug_info(&db),
        ),
        &source_file,
    );
    let aliases = copy_bindings(&db, &program, &source_file);

    // Sierra var -> value, per function, in execution order.
    let sierra_values = run_and_read_arguments(&program)?;

    // Declaration -> (name, value): first value the program held.
    let mut by_decl: HashMap<DeclPos, (String, i64)> = HashMap::new();
    for (func_id, var_id, value) in sierra_values {
        let Some((name, pos)) = debug_names.get(&(func_id, var_id)) else {
            continue;
        };
        by_decl.entry(*pos).or_insert_with(|| (name.clone(), value));
    }

    // A copy binding `let x = y;` holds the same value as `y`.
    let mut changed = true;
    while changed {
        changed = false;
        for alias in &aliases {
            let target = by_decl.get(&alias.target).map(|(_, v)| *v);
            let source = by_decl.get(&alias.source).map(|(_, v)| *v);
            match (target, source) {
                (None, Some(v)) => {
                    by_decl.insert(alias.target, (alias.target_name.clone(), v));
                    changed = true;
                }
                (Some(v), None) => {
                    by_decl.insert(alias.source, (alias.source_name.clone(), v));
                    changed = true;
                }
                _ => {}
            }
        }
    }

    let values = by_decl
        .into_iter()
        .map(|((line, _col), (name, value))| ((name, line as u32 + 1), value))
        .collect();
    Ok(ObservedLocals { values })
}

/// Parse the compiler's function debug info into
/// `(sierra function id, sierra var id) -> (cairo name, declaration)`,
/// keeping only locals declared in `source_file`.
fn sierra_variable_names(
    annotations: &Annotations,
    source_file: &Path,
) -> HashMap<(u64, u64), (String, DeclPos)> {
    let mut out = HashMap::new();
    let Some(functions) = annotations
        .get("github.com/software-mansion-labs/cairo-debugger")
        .and_then(|v| v.get("functions_info"))
        .and_then(|v| v.as_object())
    else {
        return out;
    };
    for (func_id, info) in functions {
        let Ok(func_id) = func_id.parse::<u64>() else {
            continue;
        };
        let in_source = info["function_file_path"]
            .as_str()
            .is_some_and(|p| same_file(Path::new(p), source_file));
        if !in_source {
            continue;
        }
        let Some(vars) = info["sierra_to_cairo_variable"].as_object() else {
            continue;
        };
        for (var_id, entry) in vars {
            let (Ok(var_id), Some(name), Some(line), Some(col)) = (
                var_id.parse::<u64>(),
                entry[0].as_str(),
                entry[1]["start"]["line"].as_u64(),
                entry[1]["start"]["col"].as_u64(),
            ) else {
                continue;
            };
            out.insert(
                (func_id, var_id),
                (name.to_string(), (line as usize, col as usize)),
            );
        }
    }
    out
}

fn same_file(a: &Path, b: &Path) -> bool {
    a == b || a.canonicalize().ok().as_deref() == Some(b)
}

/// A `let target = source;` binding whose right-hand side is a plain
/// local: the compiler binds `target` to the same value as `source`.
struct CopyBinding {
    target: DeclPos,
    target_name: String,
    source: DeclPos,
    source_name: String,
}

/// Collect the copy bindings of every user function of `source_file`
/// that is part of `program`, from the compiler's semantic model.
fn copy_bindings(db: &RootDatabase, program: &Program, source_file: &Path) -> Vec<CopyBinding> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for func in &program.funcs {
        let lowering_id = db.lookup_sierra_function(&func.id);
        let Ok(Some(concrete)) = lowering_id.body(db) else {
            continue;
        };
        let semantic_fn = concrete.base_semantic_function(db);
        if !seen.insert(semantic_fn) {
            continue;
        }
        let Ok(body) = db.function_body(semantic_fn.function_with_body_id(db)) else {
            continue;
        };
        for (_, statement) in body.arenas.statements.iter() {
            let Statement::Let(let_stmt) = statement else {
                continue;
            };
            let Pattern::Variable(target) = &body.arenas.patterns[let_stmt.pattern] else {
                continue;
            };
            let Expr::Var(rhs) = &body.arenas.exprs[let_stmt.expr] else {
                continue;
            };
            let SemanticVarId::Local(source_id) = rhs.var else {
                continue;
            };
            let target_var = SemanticVarId::Local(target.var.id);
            let source_var = SemanticVarId::Local(source_id);
            let (Some((target_pos, target_name)), Some((source_pos, source_name))) = (
                declaration_position(db, target_var, source_file),
                declaration_position(db, source_var, source_file),
            ) else {
                continue;
            };
            out.push(CopyBinding {
                target: target_pos,
                target_name,
                source: source_pos,
                source_name,
            });
        }
    }
    out
}

/// 0-based position and name of a local's declaring identifier, when
/// it is declared in `source_file`.
fn declaration_position(
    db: &RootDatabase,
    var: SemanticVarId<'_>,
    source_file: &Path,
) -> Option<(DeclPos, String)> {
    let location: StableLocation<'_> = var.stable_location(db);
    let user = location.span_in_file(db).user_location(db);
    if !same_file(Path::new(&user.file_id.full_path(db)), source_file) {
        return None;
    }
    let pos = user.span.position_in_file(db, user.file_id)?;
    let name = user.span.take(db.file_content(user.file_id)?).to_string();
    Some(((pos.start.line, pos.start.col), name))
}

/// Run `program`'s `main` on the Cairo VM with the trace enabled and
/// return `(sierra function id, sierra var id, value)` for every
/// scalar argument of every executed Sierra statement, in execution
/// order.
fn run_and_read_arguments(program: &Program) -> eyre::Result<Vec<(u64, u64, i64)>> {
    let builder = RunnableBuilder::new(program.clone(), None)
        .map_err(|e| eyre::eyre!("failed to build runnable: {e}"))?;
    let runner = SierraCasmRunner::new(program.clone(), None, OrderedHashMap::default(), None)
        .map_err(|e| eyre::eyre!("failed to create runner: {e}"))?;
    let main_func = program
        .funcs
        .iter()
        .find(|f| f.id.to_string().ends_with("::main"))
        .or_else(|| {
            program
                .funcs
                .iter()
                .find(|f| f.id.to_string().contains("::main"))
        })
        .ok_or_else(|| eyre::eyre!("no main function"))?;
    let (mut hint_processor, ctx) = runner
        .prepare_starknet_context(main_func, vec![], None, Default::default())
        .map_err(|e| eyre::eyre!("failed to prepare run: {e}"))?;
    let data_len = ctx.bytecode.len();
    let run = run_function(
        ctx.bytecode.iter(),
        ctx.builtins,
        |vm| initialize_vm(vm, data_len),
        &mut hint_processor,
        ctx.hints_dict,
    )
    .map_err(|e| eyre::eyre!("unoptimised run failed: {e}"))?;

    let scalar_types: std::collections::HashSet<_> = program
        .type_declarations
        .iter()
        .filter(|decl| {
            is_scalar_integer(
                decl.long_id.generic_id.0.as_str(),
                &decl.long_id.generic_args,
            )
        })
        .map(|decl| decl.id.clone())
        .collect();

    let casm = builder.casm_program();
    let infos = &casm.debug_info.sierra_statement_info;
    let casm_len: usize = infos.last().map(|i| i.end_offset).unwrap_or(0);

    // Statements sharing a start offset (zero-instruction statements
    // followed by the first statement that emits code) all execute at
    // that offset with the same `ap`/`fp`.
    let mut by_offset: HashMap<usize, Vec<usize>> = HashMap::new();
    for (idx, info) in infos.iter().enumerate() {
        by_offset.entry(info.start_offset).or_default().push(idx);
    }

    // Function owning each statement: the one with the greatest entry
    // point not past it.
    let mut entries: Vec<(usize, u64)> = program
        .funcs
        .iter()
        .map(|f| (f.entry_point.0, f.id.id))
        .collect();
    entries.sort();
    let owner = |stmt: usize| -> Option<u64> {
        let pos = entries.partition_point(|(entry, _)| *entry <= stmt);
        pos.checked_sub(1).map(|p| entries[p].1)
    };

    // The entry code ends with the `ret` that the last trace entry
    // executes; the program is loaded right after it.
    let header_end = run
        .relocated_trace
        .last()
        .map(|e| e.pc)
        .ok_or_else(|| eyre::eyre!("empty trace"))?;
    let load_offset = header_end + 1;

    let memory = &run.memory;
    let mut out = Vec::new();
    for entry in &run.relocated_trace {
        let Some(real_pc) = entry.pc.checked_sub(load_offset) else {
            continue;
        };
        if real_pc >= casm_len {
            continue;
        }
        let Some(stmts) = by_offset.get(&real_pc) else {
            continue;
        };
        for &stmt in stmts {
            let args: &[cairo_lang_sierra::ids::VarId] = match &program.statements[stmt] {
                SierraStatement::Invocation(inv) => &inv.args,
                SierraStatement::Return(vars) => vars,
            };
            let refs = match &infos[stmt].additional_kind_info {
                StatementKindDebugInfo::Invoke(info) => &info.ref_values,
                StatementKindDebugInfo::Return(info) => &info.ref_values,
            };
            let Some(func_id) = owner(stmt) else {
                continue;
            };
            for (var, reference) in args.iter().zip(refs.iter()) {
                if !scalar_types.contains(&reference.ty) {
                    continue;
                }
                let [cell] = reference.expression.cells.as_slice() else {
                    continue;
                };
                let Some(value) = eval_cell(cell, entry.ap, entry.fp, memory) else {
                    continue;
                };
                if let Some(value) = felt_to_i64(&value) {
                    out.push((func_id, var.id, value));
                }
            }
        }
    }
    Ok(out)
}

/// Whether a Sierra type is a single-cell integer (`felt252`, `uN`,
/// `iN`) whose value maps onto a Cairo integer local.
fn is_scalar_integer(generic_id: &str, generic_args: &[GenericArg]) -> bool {
    generic_args.is_empty()
        && matches!(
            generic_id,
            "felt252"
                | "u8"
                | "u16"
                | "u32"
                | "u64"
                | "u128"
                | "i8"
                | "i16"
                | "i32"
                | "i64"
                | "i128"
        )
}

fn read(memory: &[Option<Felt252>], addr: usize) -> Option<Felt252> {
    memory.get(addr).copied().flatten()
}

fn cell_addr(cell: &CellRef, ap: usize, fp: usize) -> Option<usize> {
    let base = match cell.register {
        Register::AP => ap,
        Register::FP => fp,
    };
    base.checked_add_signed(cell.offset as isize)
}

fn felt_to_usize(value: Felt252) -> Option<usize> {
    let big = value.to_biguint();
    usize::try_from(big).ok()
}

/// Evaluate a CASM cell expression against the (write-once) VM memory
/// with the registers of one trace entry.
fn eval_cell(
    cell: &CellExpression,
    ap: usize,
    fp: usize,
    memory: &[Option<Felt252>],
) -> Option<Felt252> {
    match cell {
        CellExpression::Deref(c) => read(memory, cell_addr(c, ap, fp)?),
        CellExpression::DoubleDeref(c, offset) => {
            let ptr = felt_to_usize(read(memory, cell_addr(c, ap, fp)?)?)?;
            read(memory, ptr.checked_add_signed(*offset as isize)?)
        }
        CellExpression::Immediate(v) => Some(Felt252::from(v.clone())),
        CellExpression::BinOp { op, a, b } => {
            let a = read(memory, cell_addr(a, ap, fp)?)?;
            let b = match b {
                DerefOrImmediate::Deref(c) => read(memory, cell_addr(c, ap, fp)?)?,
                DerefOrImmediate::Immediate(v) => Felt252::from(v.value.clone()),
            };
            Some(match op {
                CellOperator::Add => a + b,
                CellOperator::Sub => a - b,
                CellOperator::Mul => a * b,
                CellOperator::Div => a.field_div(&b.try_into().ok()?),
            })
        }
    }
}

/// Signed interpretation of a field element (values above p/2 are
/// negative), when it fits in an `i64`.
fn felt_to_i64(value: &Felt252) -> Option<i64> {
    let unsigned: BigUint = value.to_biguint();
    let prime: BigUint = Felt252::MAX.to_biguint() + 1u32;
    let signed: BigInt = if unsigned > &prime / 2u32 {
        BigInt::from(unsigned) - BigInt::from(prime)
    } else {
        BigInt::from(unsigned)
    };
    i64::try_from(signed).ok()
}
