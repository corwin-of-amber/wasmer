use crate::common::get_cache_dir;
#[cfg(feature = "debug")]
use crate::logging;
use crate::store::{CompilerType, StoreOptions};
use crate::suggestions::suggest_function_exports;
use crate::warning;
use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;
use std::str::FromStr;
use wasmer::FunctionEnv;
use wasmer::*;
#[cfg(feature = "cache")]
use wasmer_cache::{Cache, FileSystemCache, Hash};
use wasmer_types::Type as ValueType;

use clap::Parser;

#[cfg(feature = "wasi")]
mod wasi;

#[cfg(feature = "wasi")]
use wasi::Wasi;

#[derive(Debug, Parser, Clone, Default)]
/// The options for the `wasmer run` subcommand
pub struct Run {
    /// Disable the cache
    #[cfg(feature = "cache")]
    #[clap(long = "disable-cache")]
    disable_cache: bool,

    /// File to run
    #[clap(name = "FILE", parse(from_os_str))]
    path: PathBuf,

    /// Invoke a specified function
    #[clap(long = "invoke", short = 'i')]
    invoke: Option<String>,

    /// The command name is a string that will override the first argument passed
    /// to the wasm program. This is used in wapm to provide nicer output in
    /// help commands and error messages of the running wasm program
    #[clap(long = "command-name", hide = true)]
    command_name: Option<String>,

    /// A prehashed string, used to speed up start times by avoiding hashing the
    /// wasm module. If the specified hash is not found, Wasmer will hash the module
    /// as if no `cache-key` argument was passed.
    #[cfg(feature = "cache")]
    #[clap(long = "cache-key", hide = true)]
    cache_key: Option<String>,

    #[clap(flatten)]
    store: StoreOptions,

    // TODO: refactor WASI structure to allow shared options with Emscripten
    #[cfg(feature = "wasi")]
    #[clap(flatten)]
    wasi: Wasi,

    /// Enable non-standard experimental IO devices
    #[cfg(feature = "io-devices")]
    #[clap(long = "enable-io-devices")]
    enable_experimental_io_devices: bool,

    /// Enable debug output
    #[cfg(feature = "debug")]
    #[clap(long = "debug", short = 'd')]
    debug: bool,

    #[cfg(feature = "debug")]
    #[clap(short, long, parse(from_occurrences))]
    verbose: u8,

    /// Application arguments
    #[clap(value_name = "ARGS")]
    args: Vec<String>,
}

impl Run {
    /// Execute the run command
    pub fn execute(&self) -> Result<()> {
        #[cfg(feature = "debug")]
        if self.debug {
            logging::set_up_logging(self.verbose).unwrap();
        }
        self.inner_execute().with_context(|| {
            format!(
                "failed to run `{}`{}",
                self.path.display(),
                if CompilerType::enabled().is_empty() {
                    " (no compilers enabled)"
                } else {
                    ""
                }
            )
        })
    }

    fn inner_module_run(&self, mut store: Store, instance: Instance) -> Result<()> {
        // If this module exports an _initialize function, run that first.
        if let Ok(initialize) = instance.exports.get_function("_initialize") {
            initialize
                .call(&mut store, &[])
                .with_context(|| "failed to run _initialize function")?;
        }

        // Do we want to invoke a function?
        if let Some(ref invoke) = self.invoke {
            let result = self.invoke_function(&mut store, &instance, invoke, &self.args)?;
            println!(
                "{}",
                result
                    .iter()
                    .map(|val| val.to_string())
                    .collect::<Vec<String>>()
                    .join(" ")
            );
        } else {
            let start: Function = self.try_find_function(&instance, "_start", &[])?;
            let result = start.call(&mut store, &[]);
            #[cfg(feature = "wasi")]
            self.wasi.handle_result(result)?;
            #[cfg(not(feature = "wasi"))]
            result?;
        }

        Ok(())
    }

    fn inner_execute(&self) -> Result<()> {
        let (mut store, module) = self.get_store_module()?;
        #[cfg(feature = "emscripten")]
        {
            use wasmer_emscripten::{
                generate_emscripten_env, is_emscripten_module, run_emscripten_instance, EmEnv,
                EmscriptenGlobals,
            };
            // TODO: refactor this
            if is_emscripten_module(&module) {
                // create an EmEnv with default global
                let env = FunctionEnv::new(&mut store, EmEnv::new());
                let mut emscripten_globals = EmscriptenGlobals::new(&mut store, &env, &module)
                    .map_err(|e| anyhow!("{}", e))?;
                env.as_mut(&mut store)
                    .set_data(&emscripten_globals.data, Default::default());
                let import_object =
                    generate_emscripten_env(&mut store, &env, &mut emscripten_globals);
                let mut instance = match Instance::new(&mut store, &module, &import_object) {
                    Ok(instance) => instance,
                    Err(e) => {
                        let err: Result<(), _> = Err(e);
                        #[cfg(feature = "wasi")]
                        {
                            if Wasi::has_wasi_imports(&module) {
                                return err.with_context(|| "This module has both Emscripten and WASI imports. Wasmer does not currently support Emscripten modules using WASI imports.");
                            }
                        }
                        return err.with_context(|| "Can't instantiate emscripten module");
                    }
                };

                run_emscripten_instance(
                    &mut instance,
                    env.into_mut(&mut store),
                    &mut emscripten_globals,
                    if let Some(cn) = &self.command_name {
                        cn
                    } else {
                        self.path.to_str().unwrap()
                    },
                    self.args.iter().map(|arg| arg.as_str()).collect(),
                    self.invoke.clone(),
                )?;
                return Ok(());
            }
        }

        // If WASI is enabled, try to execute it with it
        #[cfg(feature = "wasi")]
        let ret = {
            use std::collections::BTreeSet;
            use wasmer_wasi::WasiVersion;

            let wasi_versions = Wasi::get_versions(&module);
            match wasi_versions {
                Some(wasi_versions) if !wasi_versions.is_empty() => {
                    if wasi_versions.len() >= 2 {
                        let get_version_list = |versions: &BTreeSet<WasiVersion>| -> String {
                            versions
                                .iter()
                                .map(|v| format!("`{}`", v.get_namespace_str()))
                                .collect::<Vec<String>>()
                                .join(", ")
                        };
                        if self.wasi.deny_multiple_wasi_versions {
                            let version_list = get_version_list(&wasi_versions);
                            bail!("Found more than 1 WASI version in this module ({}) and `--deny-multiple-wasi-versions` is enabled.", version_list);
                        } else if !self.wasi.allow_multiple_wasi_versions {
                            let version_list = get_version_list(&wasi_versions);
                            warning!("Found more than 1 WASI version in this module ({}). If this is intentional, pass `--allow-multiple-wasi-versions` to suppress this warning.", version_list);
                        }
                    }

                    let program_name = self
                        .command_name
                        .clone()
                        .or_else(|| {
                            self.path
                                .file_name()
                                .map(|f| f.to_string_lossy().to_string())
                        })
                        .unwrap_or_default();
                    let (_ctx, instance) = self
                        .wasi
                        .instantiate(&mut store, &module, program_name, self.args.clone())
                        .with_context(|| "failed to instantiate WASI module")?;
                    self.inner_module_run(store, instance)
                }
                // not WASI
                _ => {
                    let instance = Instance::new(&mut store, &module, &imports! {})?;
                    self.inner_module_run(store, instance)
                }
            }
        };
        #[cfg(not(feature = "wasi"))]
        let ret = {
            let instance = Instance::new(&module, &imports! {})?;

            // If this module exports an _initialize function, run that first.
            if let Ok(initialize) = instance.exports.get_function("_initialize") {
                initialize
                    .call(&[])
                    .with_context(|| "failed to run _initialize function")?;
            }

            // Do we want to invoke a function?
            if let Some(ref invoke) = self.invoke {
                let result = self.invoke_function(&instance, invoke, &self.args)?;
                println!(
                    "{}",
                    result
                        .iter()
                        .map(|val| val.to_string())
                        .collect::<Vec<String>>()
                        .join(" ")
                );
            } else {
                let start: Function = self.try_find_function(&instance, "_start", &[])?;
                let result = start.call(&[]);
                #[cfg(feature = "wasi")]
                self.wasi.handle_result(result)?;
                #[cfg(not(feature = "wasi"))]
                result?;
            }
        };

        ret
    }

    fn get_store_module(&self) -> Result<(Store, Module)> {
        let contents = std::fs::read(self.path.clone())?;
        if wasmer_compiler::Artifact::is_deserializable(&contents) {
            let engine = wasmer_compiler::EngineBuilder::headless();
            let store = Store::new(engine);
            let module = unsafe { Module::deserialize_from_file(&store, &self.path)? };
            return Ok((store, module));
        }
        let (store, compiler_type) = self.store.get_store()?;
        #[cfg(feature = "cache")]
        let module_result: Result<Module> = if !self.disable_cache && contents.len() > 0x1000 {
            self.get_module_from_cache(&store, &contents, &compiler_type)
        } else {
            Module::new(&store, contents).map_err(|e| e.into())
        };
        #[cfg(not(feature = "cache"))]
        let module_result = Module::new(&store, &contents);

        let mut module = module_result.with_context(|| {
            format!(
                "module instantiation failed (compiler: {})",
                compiler_type.to_string()
            )
        })?;
        // We set the name outside the cache, to make sure we dont cache the name
        module.set_name(&self.path.file_name().unwrap_or_default().to_string_lossy());

        Ok((store, module))
    }

    #[cfg(feature = "cache")]
    fn get_module_from_cache(
        &self,
        store: &Store,
        contents: &[u8],
        compiler_type: &CompilerType,
    ) -> Result<Module> {
        // We try to get it from cache, in case caching is enabled
        // and the file length is greater than 4KB.
        // For files smaller than 4KB caching is not worth,
        // as it takes space and the speedup is minimal.
        let mut cache = self.get_cache(compiler_type)?;
        // Try to get the hash from the provided `--cache-key`, otherwise
        // generate one from the provided file `.wasm` contents.
        let hash = self
            .cache_key
            .as_ref()
            .and_then(|key| Hash::from_str(key).ok())
            .unwrap_or_else(|| Hash::generate(contents));
        match unsafe { cache.load(store, hash) } {
            Ok(module) => Ok(module),
            Err(e) => {
                match e {
                    DeserializeError::Io(_) => {
                        // Do not notify on IO errors
                    }
                    err => {
                        warning!("cached module is corrupted: {}", err);
                    }
                }
                let module = Module::new(store, contents)?;
                // Store the compiled Module in cache
                cache.store(hash, &module)?;
                Ok(module)
            }
        }
    }

    #[cfg(feature = "cache")]
    /// Get the Compiler Filesystem cache
    fn get_cache(&self, compiler_type: &CompilerType) -> Result<FileSystemCache> {
        let mut cache_dir_root = get_cache_dir();
        cache_dir_root.push(compiler_type.to_string());
        let mut cache = FileSystemCache::new(cache_dir_root)?;

        let extension = "wasmu";
        cache.set_cache_extension(Some(extension));
        Ok(cache)
    }

    fn try_find_function(
        &self,
        instance: &Instance,
        name: &str,
        args: &[String],
    ) -> Result<Function> {
        Ok(instance
            .exports
            .get_function(name)
            .map_err(|e| {
                if instance.module().info().functions.is_empty() {
                    anyhow!("The module has no exported functions to call.")
                } else {
                    let suggested_functions = suggest_function_exports(instance.module(), "");
                    let names = suggested_functions
                        .iter()
                        .take(3)
                        .map(|arg| format!("`{}`", arg))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let suggested_command = format!(
                        "wasmer {} -i {} {}",
                        self.path.display(),
                        suggested_functions.get(0).unwrap_or(&String::new()),
                        args.join(" ")
                    );
                    let suggestion = if suggested_functions.is_empty() {
                        String::from("Can not find any export functions.")
                    } else {
                        format!(
                            "Similar functions found: {}.\nTry with: {}",
                            names, suggested_command
                        )
                    };
                    match e {
                        ExportError::Missing(_) => {
                            anyhow!("No export `{}` found in the module.\n{}", name, suggestion)
                        }
                        ExportError::IncompatibleType => anyhow!(
                            "Export `{}` found, but is not a function.\n{}",
                            name,
                            suggestion
                        ),
                    }
                }
            })?
            .clone())
    }

    fn invoke_function(
        &self,
        ctx: &mut impl AsStoreMut,
        instance: &Instance,
        invoke: &str,
        args: &[String],
    ) -> Result<Box<[Value]>> {
        let func: Function = self.try_find_function(instance, invoke, args)?;
        let func_ty = func.ty(ctx);
        let required_arguments = func_ty.params().len();
        let provided_arguments = args.len();
        if required_arguments != provided_arguments {
            bail!(
                "Function expected {} arguments, but received {}: \"{}\"",
                required_arguments,
                provided_arguments,
                self.args.join(" ")
            );
        }
        let invoke_args = args
            .iter()
            .zip(func_ty.params().iter())
            .map(|(arg, param_type)| match param_type {
                ValueType::I32 => {
                    Ok(Value::I32(arg.parse().map_err(|_| {
                        anyhow!("Can't convert `{}` into a i32", arg)
                    })?))
                }
                ValueType::I64 => {
                    Ok(Value::I64(arg.parse().map_err(|_| {
                        anyhow!("Can't convert `{}` into a i64", arg)
                    })?))
                }
                ValueType::F32 => {
                    Ok(Value::F32(arg.parse().map_err(|_| {
                        anyhow!("Can't convert `{}` into a f32", arg)
                    })?))
                }
                ValueType::F64 => {
                    Ok(Value::F64(arg.parse().map_err(|_| {
                        anyhow!("Can't convert `{}` into a f64", arg)
                    })?))
                }
                _ => Err(anyhow!(
                    "Don't know how to convert {} into {:?}",
                    arg,
                    param_type
                )),
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(func.call(ctx, &invoke_args)?)
    }

    /// Create Run instance for arguments/env,
    /// assuming we're being run from a CFP binfmt interpreter.
    pub fn from_binfmt_args() -> Run {
        Self::from_binfmt_args_fallible().unwrap_or_else(|e| {
            crate::error::PrettyError::report::<()>(
                Err(e).context("Failed to set up wasmer binfmt invocation"),
            )
        })
    }

    #[cfg(target_os = "linux")]
    fn from_binfmt_args_fallible() -> Result<Run> {
        let argv = std::env::args_os().collect::<Vec<_>>();
        let (_interpreter, executable, original_executable, args) = match &argv[..] {
            [a, b, c, d @ ..] => (a, b, c, d),
            _ => {
                bail!("Wasmer binfmt interpreter needs at least three arguments (including $0) - must be registered as binfmt interpreter with the CFP flags. (Got arguments: {:?})", argv);
            }
        };
        // TODO: Optimally, args and env would be passed as an UTF-8 Vec.
        // (Can be pulled out of std::os::unix::ffi::OsStrExt)
        // But I don't want to duplicate or rewrite run.rs today.
        let args = args
            .iter()
            .enumerate()
            .map(|(i, s)| {
                s.clone().into_string().map_err(|s| {
                    anyhow!(
                        "Cannot convert argument {} ({:?}) to UTF-8 string",
                        i + 1,
                        s
                    )
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let original_executable = original_executable
            .clone()
            .into_string()
            .map_err(|s| anyhow!("Cannot convert executable name {:?} to UTF-8 string", s))?;
        let store = StoreOptions::default();
        // TODO: store.compiler.features.all = true; ?
        Ok(Self {
            args,
            path: executable.into(),
            command_name: Some(original_executable),
            store,
            wasi: Wasi::for_binfmt_interpreter()?,
            ..Self::default()
        })
    }
    #[cfg(not(target_os = "linux"))]
    fn from_binfmt_args_fallible() -> Result<Run> {
        bail!("binfmt_misc is only available on linux.")
    }
}
