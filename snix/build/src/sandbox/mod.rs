use std::path::{Path, PathBuf};

use typed_builder::TypedBuilder;

use crate::buildservice::{AdditionalFile, EnvVar};

pub mod event;

/// A handle returned by sandboxes that allows finding build outputs relative to logical sandbox root.
///
/// Since different build sandboxes have their own internal directory layout, this trait
/// encapsulates those implementation details and allows callers to query paths in a consistent way.
pub(crate) trait SandboxOutputs {
    /// Resolves to an absolute filesystem host path that corresponds to `path`.
    fn find_path(
        &self,
        path: impl AsRef<Path> + Send,
    ) -> impl Future<Output = Option<PathBuf>> + Send;
}

/// A sandbox builder.
///
/// Its API is tailored to the needs of Snix builds, namely running sandboxed commands
/// with optional build input paths, files, network access. And allow for such commands
/// to produce outputs that stay available after the sandbox has stopped.
#[derive(TypedBuilder)]
pub struct SandboxSpec {
    /// Working directory on the host, where the sandbox is assembled.
    #[builder(setter(into))]
    host_workdir: PathBuf,

    /// Command to execute inside the sandbox
    #[builder(setter(fn transform<I, P>(value: I) -> Vec<String>
            where
                I: IntoIterator<Item = P>,
                P: AsRef<str>,
            {
                value.into_iter().map(|p| p.as_ref().into()).collect()
            }))]
    command: Vec<String>,

    /// Workdir inside the sandbox, in which the [Self::command] will be executed.
    #[builder(setter(into))]
    sandbox_workdir: PathBuf,

    /// A list of scratch paths to make available inside the sandbox.
    ///
    /// These directories are read+writable inside the sandbox and their contents is preserved
    /// after the sandbox has stopped.
    #[builder(setter(fn transform<I, P>(value: I) -> Vec<PathBuf>
            where
                I: IntoIterator<Item = P>,
                P: AsRef<Path>,
            {
                value.into_iter().map(|p| p.as_ref().into()).collect()
            }))]
    scratches: Vec<PathBuf>,

    /// Any additional files to rw-mount inside the sandbox.
    #[builder(default, setter(into))]
    additional_files: Vec<AdditionalFile>,

    /// Env vars to set before running [Self::command].
    #[builder(default, setter(into))]
    env_vars: Vec<EnvVar>,

    /// Optionally read-only mount build inputs.
    ///
    /// # Example
    /// Mount some host path at "/nix/store" inside the sandbox.
    ///
    /// ```rust
    /// use snix_build::sandbox::SandboxSpec;
    /// let _  = SandboxSpec::builder()
    ///     .host_workdir("/tmp/sandbox1")
    ///     .command(["echo", "Hello"])
    ///     .sandbox_workdir("build")
    ///     .scratches(["foo"])
    ///     .with_inputs("nix/store", |path| {
    ///         // mount dir at `path`
    ///         // return an RAII guard that will unmount the dir
    ///         Ok(())
    ///     })
    ///     .build();
    /// ```
    #[builder(default, setter(
        fn transform<TResult: InputsGuard + 'static>(
             inputs_dir: impl AsRef<Path>,
             provider: impl Fn(&Path) -> std::io::Result<TResult> + Send + 'static,
        ) -> InputsProvider {
            InputsProvider::new(inputs_dir, provider)
        }
    ))]
    with_inputs: InputsProvider,

    /// Absolute path to the shell that will be mounted at /bin/sh inside the sandbox.
    ///
    /// It must static binary, otherwise it will likely fail to start.
    #[builder(default)]
    provide_shell: Option<PathBuf>,

    /// Whether to allow network access inside the sandbox.
    #[builder(default)]
    allow_network: bool,
}

impl SandboxSpec {
    pub fn host_workdir(&self) -> &Path {
        &self.host_workdir
    }

    pub fn command(&self) -> impl IntoIterator<Item = &String> {
        &self.command
    }

    pub fn sandbox_workdir(&self) -> &Path {
        &self.sandbox_workdir
    }

    pub fn scratches(&self) -> impl IntoIterator<Item = &PathBuf> {
        &self.scratches
    }

    pub fn additional_files(&self) -> impl IntoIterator<Item = &AdditionalFile> {
        &self.additional_files
    }

    pub fn env_vars(&self) -> impl IntoIterator<Item = &EnvVar> {
        &self.env_vars
    }

    pub fn provide_shell(&self) -> Option<&Path> {
        self.provide_shell.as_deref()
    }

    pub fn allow_network(&self) -> bool {
        self.allow_network
    }

    pub fn inputs_provider(&self) -> &InputsProvider {
        &self.with_inputs
    }
}

/// Inputs provider.
pub struct InputsProvider {
    inputs_dir: PathBuf,
    provider: ProviderFn,
}

impl InputsProvider {
    fn new<TResult: Send + 'static>(
        inputs_dir: impl AsRef<Path>,
        mut provider: impl FnMut(&Path) -> std::io::Result<TResult> + Send + 'static,
    ) -> Self {
        Self {
            inputs_dir: inputs_dir.as_ref().into(),
            provider: Box::new(move |p| provider(p).map(|r| Box::new(r) as Box<dyn InputsGuard>)),
        }
    }

    /// Consumes the inputs provider and provides the inputs at `path`.
    ///
    /// The returned guard cleans up or unmounts inputs when dropped.
    pub fn provide_inputs(
        mut self,
        path: impl AsRef<Path>,
    ) -> std::io::Result<Box<dyn InputsGuard>> {
        (self.provider)(path.as_ref())
    }

    pub fn inputs_dir(&self) -> &Path {
        &self.inputs_dir
    }
}

impl Default for InputsProvider {
    fn default() -> Self {
        Self {
            inputs_dir: Default::default(),
            provider: Box::new(|_| Ok(Box::new(()))),
        }
    }
}

impl From<SandboxSpec> for InputsProvider {
    fn from(value: SandboxSpec) -> Self {
        value.with_inputs
    }
}

/// RAII token for the inputs.
///
/// When this guard is dropped, the inputs may be unmounted/deleted.
pub trait InputsGuard: Send {}

/// Blanket implementation for all types.
///
/// It's up to the inputs provider whether it wants to unmount/delete inputs.
impl<T: Send> InputsGuard for T {}

/// Type erased closure providing sandbox inputs.
///
/// Returns an guard that has a chance to clean up/unmount inputs after the sandbox has stopped.
type ProviderFn = Box<dyn FnMut(&Path) -> std::io::Result<Box<dyn InputsGuard>> + Send>;

#[doc(hidden)]
/// When nothing is set, you can't call build():
///
/// ```compile_fail
/// use snix_build::sandbox::SandboxSpec;
/// let _ = SandboxSpec::builder().build();
///
/// ```
///
/// When all required fields are set, can build():
/// ```rust
/// use snix_build::sandbox::SandboxSpec;
/// let _ = SandboxSpec::builder()
///     .host_workdir("/tmp/foo")
///     .command(["/bin/sh", "-c", "echo Hello"])
///     .sandbox_workdir("/build")
///     .scratches(["build", "nix/store"])
///     .build();
/// ```
///
/// Can't call provide_inputs twice:
///
/// Compile fails
/// ```compile_fail
/// use snix_build::sandbox::InputsProvider;
///
/// fn test_inputs_provider(p: InputsProvider) {
///   let guard1 = p.provide_inputs("/tmp");
///   let guard2 = p.provide_inputs("/tmp");
/// }
/// ```
fn _compile_tests() {}
