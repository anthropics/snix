#![cfg_attr(docsrs, feature(doc_cfg))]

use std::path::PathBuf;
use std::rc::Rc;
use std::{fmt::Write, sync::Arc};

use rustc_hash::FxHashMap;
use smol_str::SmolStr;
use snix_build::buildservice;
use snix_castore::blob_engine::BlobServiceEngine;
use snix_eval::{
    ErrorKind, EvalIO, EvalMode, GlobalsMap, SourceCode, Value,
    builtins::impure_builtins,
    observer::{DisassemblingObserver, TracingObserver},
};
use snix_glue::{
    builtins::{add_derivation_builtins, add_fetcher_builtins, add_import_builtins},
    configure_nix_path,
    snix_io::SnixIO,
    snix_store_io::SnixStoreIO,
};
use tracing::{Span, info_span};
use tracing_indicatif::span_ext::IndicatifSpanExt;

pub mod args;
pub mod assignment;
pub mod repl;

pub use args::Args;
pub use repl::Repl;

pub async fn init_io_handle(args: &Args) -> SnixStoreIO {
    let (blob_service, directory_service, path_info_service, nar_calculation_service) =
        snix_store::utils::construct_services(args.service_addrs.clone())
            .await
            .expect("unable to setup {blob|directory|pathinfo}service before interpreter setup");

    let blob_engine = Arc::new(BlobServiceEngine(blob_service));

    let build_service = buildservice::from_addr(
        &args.build_service_addr,
        blob_engine.clone(),
        directory_service.clone(),
    )
    .await
    .expect("unable to setup buildservice before interpreter setup");

    SnixStoreIO::new(
        blob_engine.clone(),
        directory_service,
        path_info_service,
        nar_calculation_service,
        build_service,
        tokio::runtime::Handle::current(),
        args.hashed_mirrors.clone(),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AllowIncomplete {
    Allow,
    #[default]
    RequireComplete,
}

impl AllowIncomplete {
    fn allow(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IncompleteInput;

pub struct EvalResult {
    value: Option<Value>,
    globals: Rc<GlobalsMap>,
}

/// Interprets the given code snippet, printing out warnings and errors and returning the result
#[allow(clippy::too_many_arguments)]
pub fn evaluate<E: std::io::Write + Clone + Send>(
    stderr: &mut E,
    snix_store_io: Rc<SnixStoreIO>,
    code: &str,
    path: Option<PathBuf>,
    args: &Args,
    allow_incomplete: AllowIncomplete,
    env: Option<&FxHashMap<SmolStr, Value>>,
    globals: Option<Rc<GlobalsMap>>,
    source_map: Option<SourceCode>,
) -> Result<EvalResult, IncompleteInput> {
    let mut eval_builder = snix_eval::Evaluation::builder(Box::new(SnixIO::new(
        snix_store_io.clone() as Rc<dyn EvalIO>,
    )) as Box<dyn EvalIO>)
    .enable_import()
    .env(env);

    if args.strict {
        eval_builder = eval_builder.mode(EvalMode::Strict);
    }

    match globals {
        Some(globals) => {
            eval_builder = eval_builder.with_globals(globals);
        }
        None => {
            eval_builder = eval_builder.add_builtins(impure_builtins());
            eval_builder = add_derivation_builtins(eval_builder, Rc::clone(&snix_store_io));
            eval_builder = add_fetcher_builtins(eval_builder, Rc::clone(&snix_store_io));
            eval_builder = add_import_builtins(eval_builder, Rc::clone(&snix_store_io));
        }
    };
    eval_builder = configure_nix_path(eval_builder, &args.nix_path());

    if let Some(source_map) = source_map {
        eval_builder = eval_builder.with_source_map(source_map);
    }

    let source_map = eval_builder.source_map().clone();

    let mut compiler_observer = DisassemblingObserver::new(source_map.clone(), stderr.clone());
    if args.dump_bytecode {
        eval_builder.set_compiler_observer(Some(&mut compiler_observer));
    }

    let mut runtime_observer = TracingObserver::new(stderr.clone());
    if args.trace_runtime {
        if args.trace_runtime_timing {
            runtime_observer.enable_timing()
        }
        eval_builder.set_runtime_observer(Some(&mut runtime_observer));
    }

    let eval = eval_builder.build();

    let span = if !args.trace_runtime && !args.dump_bytecode {
        info_span!("evaluate", indicatif.pb_show = tracing::field::Empty)
    } else {
        info_span!("evaluate", indicatif.pb_hide = tracing::field::Empty)
    };
    let (result, globals) = span.in_scope(|| {
        let span = Span::current();

        span.pb_set_message("Evaluating…");
        span.pb_start();

        let globals = eval.globals();
        let result = eval.evaluate(code, path);
        (result, globals)
    });
    drop(span);

    if allow_incomplete.allow()
        && result.errors.iter().any(|err| {
            matches!(
                &err.kind,
                ErrorKind::ParseErrors(pes)
                    if pes.iter().any(|pe| matches!(pe, rnix::ParseError::UnexpectedEOF))
            )
        })
    {
        return Err(IncompleteInput);
    }

    if args.display_ast
        && let Some(ref expr) = result.expr
    {
        writeln!(stderr, "AST: {}", snix_eval::pretty_print_expr(expr)).unwrap();
    }

    for error in &result.errors {
        error.fancy_format_write(stderr);
    }

    if !args.no_warnings {
        for warning in &result.warnings {
            warning.fancy_format_write(stderr, &source_map);
        }
    }

    if let Some(dumpdir) = &args.drv_dumpdir {
        // Dump all known derivations files to `dumpdir`.
        std::fs::create_dir_all(dumpdir).expect("failed to create drv dumpdir");

        for (drv_path, drv) in snix_store_io
            .build_state
            .known_paths
            .borrow()
            .get_derivations()
        {
            match std::fs::File::create_new(dumpdir.join(drv_path.to_string())) {
                Ok(mut f) => {
                    use std::io::Write;
                    drv.serialize(&mut f).expect("writing to .drv file");
                    f.flush().expect("flushing .drv file");
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    // Skip already dumped derivations.
                    continue;
                }
                Err(err) => panic!("{}", err),
            }
        }
    }

    Ok(EvalResult {
        globals,
        value: result.value,
    })
}

pub struct InterpretResult {
    output: String,
    success: bool,
    pub(crate) globals: Option<Rc<GlobalsMap>>,
}

impl InterpretResult {
    pub fn empty_success(globals: Option<Rc<GlobalsMap>>) -> Self {
        Self {
            output: String::new(),
            success: true,
            globals,
        }
    }

    pub fn finalize<E: std::io::Write>(self, stderr: &mut E) -> bool {
        write!(stderr, "{}", self.output).unwrap();
        self.success
    }

    pub fn output(&self) -> &str {
        &self.output
    }

    pub fn success(&self) -> bool {
        self.success
    }
}

/// Interprets the given code snippet, printing out warnings, errors
/// and the result itself. The return value indicates whether
/// evaluation succeeded.
#[allow(clippy::too_many_arguments)]
pub fn interpret<E: std::io::Write + Clone + Send>(
    stderr: &mut E,
    snix_store_io: Rc<SnixStoreIO>,
    code: &str,
    path: Option<PathBuf>,
    args: &Args,
    explain: bool,
    allow_incomplete: AllowIncomplete,
    env: Option<&FxHashMap<SmolStr, Value>>,
    globals: Option<Rc<GlobalsMap>>,
    source_map: Option<SourceCode>,
) -> Result<InterpretResult, IncompleteInput> {
    let mut output = String::new();
    let result = evaluate(
        stderr,
        snix_store_io,
        code,
        path,
        args,
        allow_incomplete,
        env,
        globals,
        source_map,
    )?;

    if let Some(value) = result.value.as_ref() {
        if explain {
            writeln!(&mut output, "=> {}", value.explain()).unwrap();
        } else if args.raw {
            writeln!(&mut output, "{value}").unwrap();
        } else {
            writeln!(&mut output, "=> {} :: {}", value, value.type_of()).unwrap();
        }
    }

    // inform the caller about any errors
    Ok(InterpretResult {
        output,
        success: result.value.is_some(),
        globals: Some(result.globals),
    })
}
