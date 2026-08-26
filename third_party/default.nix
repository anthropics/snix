# This file defines the root of all external dependency imports (i.e.
# third-party code) in the TVL package tree.
#
# There are two categories of third-party programs:
#
# 1) Programs in nixpkgs, the NixOS package set. For these, you might
#    want to look at //third_party/nixpkgs (for the package set
#    imports) and //third_party/overlays (for modifications in these
#    imported package sets).
#
# 2) Third-party software packaged in this repository. This is all
#    other folders below //third_party, other than the ones mentioned
#    above.

{ ... }: { }
