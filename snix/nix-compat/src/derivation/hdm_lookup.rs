use crate::{nixhash::Sha256, store_path::StorePathRef};

/// Lookup hash derivation modulo for a derivation.
///
/// The [hash derivation modulo] is recursive. And so to calculate the HDM of a
/// derivation it requires the HDM for all its input derviations.
///
/// This trait exists to provide the HDM values for those input derivations to
/// the functions that calculate the HDM.
///
/// To help with implemeting this trait [`lookup_fn`] is provided.
///
/// [hash derivation module]: nix_compat::derivation#hash-derivation-module
pub trait HashDerivationModuloLookup {
    /// Lookup the [hash derivation modulo] of the provided derivation store path.
    ///
    /// [hash derivation module]: nix_compat::derivation#hash-derivation-module
    fn lookup_hdm(&self, drv_path: &StorePathRef<'_>) -> Option<Sha256>;
}

/// Implement [`HashDerivationModuloLookup`] using the provided closure.
pub fn lookup_fn<F>(func: F) -> impl HashDerivationModuloLookup
where
    F: Fn(&StorePathRef) -> Option<Sha256>,
{
    LookupFn { func }
}

struct LookupFn<F> {
    func: F,
}

impl<F> HashDerivationModuloLookup for LookupFn<F>
where
    F: Fn(&StorePathRef) -> Option<Sha256>,
{
    fn lookup_hdm(&self, drv_path: &StorePathRef) -> Option<Sha256> {
        (self.func)(drv_path)
    }
}

#[cfg(feature = "hashbrown")]
impl HashDerivationModuloLookup for hashbrown::HashMap<crate::store_path::StorePath, Sha256> {
    fn lookup_hdm(&self, drv_path: &StorePathRef<'_>) -> Option<Sha256> {
        self.get(drv_path).copied()
    }
}
