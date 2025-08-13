# Conda History Implementation Plan for Rattler

## Overview
Port conda's history functionality to Rust, providing a type-safe API for reading, writing, and managing conda environment history files. The implementation will be in the `rattler_conda_types` crate as it defines core conda data types.

## Reference Resources

### Conda Source Code
- **Primary Reference**: [`conda/history.py`](https://github.com/conda/conda/blob/main/conda/history.py) - The canonical implementation
- **Tests**: [`conda/tests/test_history.py`](https://github.com/conda/conda/blob/main/conda/tests/test_history.py) - Test cases showing expected behavior
- **Models**: [`conda/models/records.py`](https://github.com/conda/conda/blob/main/conda/models/records.py) - Related data structures

### Documentation
- [Conda Environment Management](https://docs.conda.io/projects/conda/en/latest/user-guide/tasks/manage-environments.html#viewing-a-list-of-revisions)
- [GitHub Issue #7852](https://github.com/conda/conda/issues/7852) - Discussion on history robustness

### Test Data
Test history files are located in `test-data/history/`:
- `simple-history` - Basic install/remove operations
- `complex-history` - Multiple revisions with updates and downgrades
- `empty-history` - New environment with no operations
- `malformed-history` - History with parsing edge cases
- `large-history` - Performance testing with many entries

## Stage 1A: Project Setup
**Goal**: Set up the module structure and basic scaffolding
**Success Criteria**: Module structure exists and compiles
**Tests**: Basic compilation tests
**Status**: Not Started

### Tasks:
- [ ] Create `history/` directory in `rattler_conda_types/src/`
- [ ] Create `history/mod.rs` with basic module exports
- [ ] Create `history/revision.rs` with placeholder structs
- [ ] Add `pub mod history;` to `rattler_conda_types/src/lib.rs`
- [ ] Ensure everything compiles

## Stage 1B: Error Types  
**Goal**: Define error handling with thiserror::Error
**Success Criteria**: Comprehensive error types that compile
**Tests**: Unit tests for error construction and display
**Status**: Not Started

### Tasks:
- [ ] Write tests first for error types
- [ ] Define `HistoryError` using `thiserror::Error` in `mod.rs`:
  - `Io(#[from] std::io::Error)` for file operations
  - `ParseError { line: usize, message: String }` for parsing failures
  - `InvalidRevision { revision: usize, max: usize }` for bounds checking
- [ ] Test error messages and formatting

## Stage 1C: Core Data Types
**Goal**: Define the fundamental data structures
**Success Criteria**: All types represent the history format accurately
**Tests**: Unit tests for type construction and serialization
**Status**: Not Started  

### Tasks:
- [ ] Write tests first for all data types
- [ ] Define `UserRequest` enum in `revision.rs`:
  - Install, Remove, Update, Create, Custom(String) 
  - With comprehensive documentation
- [ ] Define `PackageChange` struct:
  - Use `PackageName`, `Version`, `Channel` from rattler_conda_types
  - Add operation field (Add/Remove)
  - Comprehensive documentation
- [ ] Define `Revision` struct:
  - Timestamp, user_request, diff (Vec<PackageChange>), command (Optional), tool_version (Optional)
  - Support tools other than conda (mamba, micromamba, etc.)
  - All fields except timestamp and user_request are optional (per conda source analysis)
  - Comprehensive documentation
- [ ] Define `History` struct basics:
  - `revisions: Vec<Revision>` field
  - Plan for Vec-like API (push, iter, FromIterator, IntoIterator)
- [ ] Test type construction with various inputs

## Stage 2A: Individual Parsing Functions
**Goal**: Implement individual parsing functions for each component
**Success Criteria**: Each parsing function handles its specific format correctly
**Tests**: TDD - unit tests for each parsing function with edge cases
**Status**: Not Started

### Tasks:
- [ ] Write tests first for `parse_timestamp()` using simple examples
- [ ] Implement `parse_timestamp()` for (==> YYYY-MM-DD HH:MM:SS <==)
- [ ] Write tests first for `parse_command()` with optional handling
- [ ] Implement `parse_command()` for (# cmd: ...) - returns Option<String>
- [ ] Write tests first for `parse_tool_version()` supporting different tools
- [ ] Implement `parse_tool_version()` for (# conda version: ..., # mamba version: ...) - returns Option<String>
- [ ] Write tests first for `parse_package_change()`
- [ ] Implement `parse_package_change()` for (+/-channel::package-version-build)
- [ ] Write tests first for `parse_user_request()` 
- [ ] Implement `parse_user_request()` from specs format - extracts UserRequest from specs

## Stage 2B: Revision Parsing
**Goal**: Combine individual parsers into full revision parsing
**Success Criteria**: Can parse complete revisions from test fixtures
**Tests**: TDD - test against simple-history and malformed-history files  
**Status**: Not Started

### Tasks:
- [ ] Write tests first for `Revision::parse()` using `simple-history`
- [ ] Implement `Revision::parse()` combining all parsing functions
- [ ] Handle optional fields gracefully (per conda source analysis)
- [ ] Add proper error handling with line numbers
- [ ] Test against `malformed-history` for error cases
- [ ] Ensure all parsing preserves original data accurately

## Stage 3A: Revision Serialization
**Goal**: Convert Revision back to conda format
**Success Criteria**: Generated format matches original conda files exactly
**Tests**: TDD - test serialization of parsed revisions matches original
**Status**: Not Started

### Tasks:
- [ ] Write tests first for `Revision::to_string()` and `Display` trait using parsed revisions
- [ ] Implement `Display` trait for `Revision` for conda compatibility:
  - This enables `write!(writer, "{}", revision)` in streaming
- [ ] Implement `Revision::to_string()` using the Display implementation
- [ ] Handle optional fields correctly in output
- [ ] Support different tools (conda, mamba, micromamba) in output
- [ ] Test round-trip: parse revision → serialize → parse again
- [ ] Ensure output exactly matches conda's format

## Stage 3B: History File Operations
**Goal**: Read and write complete history files
**Success Criteria**: Can load/save history files reliably
**Tests**: TDD - file I/O tests using all test fixtures  
**Status**: Not Started

### Tasks:
- [ ] Write tests first for `History::from_path()` and basic operations
- [ ] Implement `History` struct in `mod.rs`:
  - `from_path(path: PathBuf)` constructor that immediately parses the file
  - `revisions: Vec<Revision>` field (no stored path)
  - `push(&mut self, revision: Revision)` to add revision to internal Vec (mimics Vec API)
  - Implement `FromIterator<Revision>` for History
  - Implement `iter()` method returning iterator over revisions
  - Implement `IntoIterator` for History and &History
- [ ] Write tests first for `History::to_path()` with streaming
- [ ] Implement `History::to_path(&self, path: &Path)` with efficient streaming:
  - Use `BufWriter` for buffered writing
  - Stream each revision using `write!(writer, "{}", revision)` (using Revision's Display trait)
  - Handle file creation and error cases
- [ ] Write tests for `History::from_prefix()` 
- [ ] Implement `History::from_prefix()` to construct from environment prefix
- [ ] Handle file creation for new environments
- [ ] Test with all files in `test-data/history/`

## Stage 4A: State Reconstruction
**Goal**: Reconstruct environment state from history revisions
**Success Criteria**: Can accurately compute package state at any revision
**Tests**: TDD - test state reconstruction using complex-history fixture
**Status**: Not Started

### Tasks:
- [ ] Write tests first for state reconstruction using `complex-history`
- [ ] Define `EnvironmentState` type as HashMap<PackageName, PackageChange>
- [ ] Implement `get_state_at_revision(index: usize)` in History
- [ ] Add revision bounds checking with clear error messages
- [ ] Test edge cases: empty history, out-of-bounds revisions

## Stage 4B: Query Features
**Goal**: Provide basic query APIs for revision information
**Success Criteria**: Can list revisions and compute diffs between them
**Tests**: TDD - query tests using all test fixtures
**Status**: Not Started

### Tasks:
- [ ] Write tests first for revision listing using test fixtures
- [ ] Implement `list_revisions()` returning revision summaries
- [ ] Write tests for diff computation using `complex-history`
- [ ] Implement `get_diff(from: usize, to: usize)` for revision comparison
- [ ] Test with all fixture files to ensure accuracy

## Stage 5A: Documentation and API Polish
**Goal**: Add comprehensive documentation and finalize public API
**Success Criteria**: All public types have excellent documentation
**Tests**: Documentation tests and examples
**Status**: Not Started

### Tasks:
- [ ] Add comprehensive rustdoc to all public types and functions
- [ ] Add usage examples in documentation
- [ ] Review and finalize public API surface
- [ ] Ensure all exports are properly added to `rattler_conda_types`

## Stage 5B: Integration with Installer
**Goal**: Integrate history tracking with rattler's installation workflow
**Success Criteria**: Installations automatically create history entries
**Tests**: Integration tests showing history is recorded during installs
**Status**: Not Started

### Tasks:
- [ ] Write integration tests first for Installer + History interaction
- [ ] Study `rattler::install::Installer` to understand integration points
- [ ] Add history tracking to installation transactions
- [ ] Ensure compatibility with existing rattler patterns
- [ ] Test end-to-end: install packages → check history file

## Stage 5C: Final Testing and Release
**Goal**: Comprehensive testing across all components
**Success Criteria**: All tests pass, ready for production use
**Tests**: Full test suite, cross-platform testing
**Status**: Not Started

### Tasks:
- [ ] Run full test suite against all `test-data/history/` files
- [ ] Cross-platform compatibility testing
- [ ] Performance testing with `large-history` file
- [ ] Final API review and cleanup
- [ ] Update rattler's public API exports

## Implementation Notes

### Key Design Decisions:
1. **Compatibility First**: Maintain 100% compatibility with conda's history format
2. **Simplicity**: Keep APIs simple and focused, avoid over-engineering
3. **Type Safety**: Use existing rattler types (`PackageName`, `Version`, `Channel`)
4. **Error Handling**: Use `thiserror::Error` for clear, contextual errors
5. **Test-Driven Development**: Write tests first for all components
6. **Documentation**: High-level docs for all public types and functions

### Integration Points:
- Use existing types from `rattler_conda_types` (`PackageName`, `Version`, `Channel`)
- Export history API from `rattler_conda_types`
- Integration with `rattler::install::Installer` for automatic history tracking
- Ensure compatibility with existing rattler patterns

### Testing Strategy:
1. **Test-Driven Development**: Write tests first for all components
2. Unit tests using test data files in `test-data/history/`:
   - `simple-history` - Basic create, install, remove operations  
   - `complex-history` - Multiple packages, channels, updates, and reverts
   - `empty-history` - New environment with no operations
   - `malformed-history` - Edge cases and parsing challenges
   - `large-history` - Performance testing with many entries
3. Round-trip serialization tests
4. Integration tests with `rattler::install::Installer`

### Future Enhancements (Post-MVP):
- History compression for old entries
- Migration tools from other package manager formats  
- Integration with conda-lock for reproducible environments
- Export to other formats (Docker, Nix, etc.)