#![allow(clippy::result_large_err)]
use std::{
    collections::HashMap,
    future::Future,
    ops::Deref,
    path::Path,
    pin::Pin,
    sync::{Arc, RwLock},
};

use anyhow::Context;
use shared_buffer::OwnedBuffer;
use virtual_fs::{AsyncReadExt, FileSystem};
use wasmer::FunctionEnvMut;
use wasmer_package::utils::from_bytes;

mod binary_package;
mod exec;

pub use self::{
    binary_package::*,
    exec::{
        import_package_mounts, package_command_by_name, run_exec, spawn_exec, spawn_exec_module,
        spawn_exec_wasm, spawn_load_module,
    },
};
use crate::{
    Runtime, SpawnError, WasiEnv,
    os::{
        command::{Commands, VirtualCommand},
        task::TaskJoinHandle,
    },
    runtime::module_cache::HashedModuleData,
};

#[derive(Debug, Clone)]
pub struct BinFactory {
    pub(crate) commands: Commands,
    runtime: Arc<dyn Runtime + Send + Sync + 'static>,
    pub(crate) local: Arc<RwLock<HashMap<String, Option<Arc<BinaryPackage>>>>>,
}

impl BinFactory {
    pub fn new(runtime: Arc<dyn Runtime + Send + Sync + 'static>) -> BinFactory {
        BinFactory {
            commands: Commands::new_with_builtins(runtime.clone()),
            runtime,
            local: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn runtime(&self) -> &(dyn Runtime + Send + Sync) {
        self.runtime.deref()
    }

    /// Register a builtin command.
    pub fn register_builtin_command<C>(&mut self, cmd: C)
    where
        C: VirtualCommand + Send + Sync + 'static,
    {
        self.commands.register_command(cmd);
    }

    /// Register a builtin command at a custom path.
    pub fn register_builtin_command_with_path<C, P>(&mut self, cmd: C, path: P)
    where
        C: VirtualCommand + Send + Sync + 'static,
        P: Into<String>,
    {
        self.commands.register_command_with_path(cmd, path.into());
    }

    /// Register a builtin command behind an [`Arc`] at a custom path.
    pub(crate) fn register_builtin_command_with_path_shared<P>(
        &mut self,
        cmd: Arc<dyn VirtualCommand + Send + Sync + 'static>,
        path: P,
    ) where
        P: Into<String>,
    {
        self.commands
            .register_command_with_path_shared(cmd, path.into());
    }

    /// Remove all registered builtin commands.
    pub fn clear_builtin_commands(&mut self) {
        self.commands.clear();
    }

    pub fn set_binary(&self, name: &str, binary: &Arc<BinaryPackage>) {
        let mut cache = self.local.write().unwrap();
        cache.insert(name.to_string(), Some(binary.clone()));
    }

    #[allow(clippy::await_holding_lock)]
    pub async fn get_binary(
        &self,
        name: &str,
        fs: Option<&dyn FileSystem>,
    ) -> Option<Arc<BinaryPackage>> {
        self.get_executable(name, fs)
            .await
            .and_then(|executable| match executable {
                Executable::Wasm(..) => None,
                Executable::BinaryPackage(pkg) => Some(pkg),
            })
    }

    pub fn spawn<'a>(
        &'a self,
        name: String,
        mut env: WasiEnv,
    ) -> Pin<Box<dyn Future<Output = Result<TaskJoinHandle, SpawnError>> + 'a>> {
        Box::pin(async move {
            // Find the binary (or die trying) and make the spawn type
            let res = self
                .get_executable(name.as_str(), Some(env.fs_root()))
                .await
                .ok_or_else(|| SpawnError::BinaryNotFound {
                    binary: name.clone(),
                });
            let executable = res?;

            // Execute
            match executable {
                Executable::Wasm(bytes, mut pre_args) => {
                    let data = HashedModuleData::new(bytes.clone());
                    apply_pre_args(&mut env, pre_args, &name);
                    spawn_exec_wasm(data, name.as_str(), env, &self.runtime).await
                }
                Executable::BinaryPackage(pkg) => {
                    {
                        let cmd = package_command_by_name(&pkg, name.as_str())?;
                        env.prepare_spawn(cmd);
                    }

                    spawn_exec(pkg.as_ref().clone(), name.as_str(), env, &self.runtime).await
                }
            }
        })
    }

    pub fn try_built_in(
        &self,
        name: String,
        parent_ctx: Option<&FunctionEnvMut<'_, WasiEnv>>,
        builder: &mut Option<WasiEnv>,
    ) -> Result<TaskJoinHandle, SpawnError> {
        // We check for built in commands
        if let Some(parent_ctx) = parent_ctx {
            if self.commands.exists(name.as_str()) {
                return self.commands.exec(parent_ctx, name.as_str(), builder);
            }
        } else if self.commands.exists(name.as_str()) {
            tracing::warn!("builtin command without a parent ctx - {}", name);
        }
        Err(SpawnError::BinaryNotFound { binary: name })
    }

    // TODO: remove allow once BinFactory is refactored
    // currently fine because a BinFactory is only used by a single process tree
    #[allow(clippy::await_holding_lock)]
    pub async fn get_executable(
        &self,
        name: &str,
        fs: Option<&dyn FileSystem>,
    ) -> Option<Executable> {
        let name = name.to_string();

        // Return early if the path is already cached
        {
            let cache = self.local.read().unwrap();
            if let Some(data) = cache.get(&name) {
                return data.clone().map(Executable::BinaryPackage);
            }
        }

        let mut cache = self.local.write().unwrap();

        // Check the cache again to avoid a race condition where the cache was populated inbetween the fast path and here
        if let Some(data) = cache.get(&name) {
            return data.clone().map(Executable::BinaryPackage);
        }

        // Check the filesystem for the file
        if name.starts_with('/')
            && let Some(fs) = fs
        {
            match load_executable_from_filesystem(fs, name.as_ref(), self.runtime()).await {
                Ok(executable) => {
                    if let Executable::BinaryPackage(pkg) = &executable {
                        cache.insert(name, Some(pkg.clone()));
                    }

                    return Some(executable);
                }
                Err(e) => {
                    tracing::warn!(
                        path = name,
                        error = &*e,
                        "Unable to load the package from disk"
                    );
                }
            }
        }

        // NAK
        cache.insert(name, None);
        None
    }
}

pub enum Executable {
    Wasm(OwnedBuffer, Vec<String>),
    BinaryPackage(Arc<BinaryPackage>),
}

async fn load_executable_from_filesystem(
    fs: &dyn FileSystem,
    path: &Path,
    rt: &(dyn Runtime + Send + Sync),
) -> Result<Executable, anyhow::Error> {
    let mut f = fs
        .new_open_options()
        .read(true)
        .open(path)
        .context("Unable to open the file")?;

    // Fast path if the file is fully available in memory.
    // Prevents redundant copying of the file data.
    let obuf: Option<OwnedBuffer> = f.as_owned_buffer();
    let bytes: Option<bytes::Bytes> = if obuf.is_some() { None } else {
        let mut data = Vec::with_capacity(f.size() as usize);
        f.read_to_end(&mut data).await.context("Read failed")?;
        Some(data.into())
    };
    let bytes_slice = &obuf.as_ref().map(|buf| &buf[..])
        .unwrap_or_else(|| &bytes.as_ref().unwrap()[..]);

    if let Some(exec) = Box::pin(shebang(fs, bytes_slice, rt)).await? {
        Ok(exec)
    }
    else if let Some(container) = container_from_slice(bytes_slice) {
        let pkg = BinaryPackage::from_webc(&container, rt)
            .await
            .context("Unable to load the package")?;

        Ok(Executable::BinaryPackage(Arc::new(pkg)))
    }
    else {
        let buf = obuf.unwrap_or_else(|| OwnedBuffer::from_bytes(bytes.unwrap()));
        Ok(Executable::Wasm(buf, vec![]))
    }
}


fn container_from_slice(bytes: &[u8]) -> Option<webc::Container> {
    // can use a let-chain when Rust edition is bumped to 2024
    if wasmer_package::utils::is_container(bytes) {
        if let Ok(container) = from_bytes(bytes.to_vec()) {
            return Some(container);
        }
    }
    None
}

async fn shebang(fs: &dyn FileSystem, bytes: &[u8], rt: &(dyn Runtime + Send + Sync)) -> Result<Option<Executable>, anyhow::Error> {
    let pfx = &bytes[0..2];
    if pfx == [b'#', b'!'] &&
            let Some(eol) = bytes.iter().position(|&x| x == b'\n') {
        let interp = String::from_utf8_lossy(&bytes[2..eol]).trim().to_owned();
        match load_executable_from_filesystem(fs, interp.as_ref(), rt).await? {
            Executable::Wasm(exe, mut pre_args) => {
                pre_args.insert(0, interp);
                Ok(Some(Executable::Wasm(exe, pre_args)))
            }
            e => Ok(Some(e))
        }
    }
    else {
        Ok(None)
    }
}

fn apply_pre_args(env: &mut WasiEnv, mut pre_args: Vec<String>, filename: &String) {
    if !pre_args.is_empty() {
        let mut state = env.state.fork();
        pre_args.push(filename.clone());  // replace argv[0] with full path
        state.args.get_mut().unwrap().splice(0..1, pre_args.drain(..));
        env.state = Arc::new(state);
    }
}
