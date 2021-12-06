# Changelog

Notable changes to this project will be documented in this file.

## Unreleased

## 0.2.0 - 12/5/21

- Added `#[externally_visible]` in conjunction with cg_nvvm dead code elimination changes to mark that
a function is externally visible.
- Added `#[address_space(...)]` in conjunction with cg_nvvm address space changes. Only meant for internal use
and advanced users.
- Added `cuda_std::ptr`.
- Added `is_in_address_space`
- Added `convert_generic_to_specific_address_space`
- Added `convert_specific_address_space_to_generic`
- Added basic static shared memory support with `cuda_std::shared_array`.
