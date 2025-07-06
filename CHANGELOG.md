# Changelog

## [Unreleased] - Git LFS-Aware Repository Cloning + Stability Fixes

### Major Improvements

#### ✨ Proper Git LFS-Aware Repository Cloning

**Problem Solved**: The previous implementation downloaded files individually via HTTP requests, which didn't properly handle Git LFS files or maintain repository integrity.

**New Approach**: 
- **Git Clone with LFS Skip**: Uses `GIT_LFS_SKIP_SMUDGE=1 git clone` to properly clone the Git repository structure first
- **LFS File Detection**: Automatically identifies Git LFS files using `git lfs ls-files`
- **Selective LFS Download**: Downloads only the actual LFS file content via HTTP resolve URLs
- **Repository Integrity**: Maintains proper Git repository structure, history, and metadata
- **Efficient Caching**: Avoids re-downloading files that are already present and complete

#### 🚀 Key Benefits

1. **Compatibility**: Mirrors the approach used by the official `huggingface_hub` Python SDK
2. **Efficiency**: Only downloads what's actually needed, skipping already-downloaded files
3. **Reliability**: Proper Git repository structure with full metadata
4. **Progress Tracking**: Maintains existing progress tracking functionality
5. **Error Handling**: Better error handling for authentication and network issues

#### 🛠️ **Stability and Hanging Prevention Fixes**

**Problem Solved**: Download processes would sometimes hang at 99.9% completion, requiring manual intervention.

**New Safety Features**:
- **Comprehensive Timeouts**: 
  - Git clone operations: 10 minute timeout
  - Git LFS file detection: 1 minute timeout  
  - Individual LFS file downloads: 10 minute timeout
  - HTTP requests: 5 minute timeout
  - Torrent creation: 30 minute timeout
- **Retry Logic**: Automatic retry (up to 3 attempts) for failed downloads with exponential backoff
- **Progress Monitoring**: Real-time progress logging every 30 seconds for large file downloads
- **Repository Verification**: Validates all expected files are present before proceeding to torrent creation
- **Graceful Error Handling**: Proper cleanup and error reporting for all failure scenarios

**User Experience Improvements**:
- **Detailed Logging**: Clear indication of which step is currently running
- **Progress Transparency**: Shows file-by-file download progress and completion status
- **Timeout Notifications**: Clear error messages when operations time out
- **100% Progress Completion**: Ensures frontend shows 100% when torrent is successfully created
- **Size Calculation Debugging**: Detailed logging to identify size mismatches and missing files
- **Accurate Progress Tracking**: Correctly accounts for both regular Git files and LFS files in progress calculation

#### 🔧 Technical Details

- **New Module**: `git_lfs_clone` module with `GitLfsCloner` struct
- **Dependencies**: Added `tokio` process feature for Git command execution
- **Progress Integration**: Seamlessly integrates with existing progress tracking system
- **Authentication Ready**: Prepared for HF token authentication (TODO: implement)

#### 📋 Implementation Highlights

- Detects if repository is already complete to avoid unnecessary work
- Handles both initial clones and updates to existing repositories
- Gracefully handles repositories that don't use Git LFS
- Provides detailed logging for debugging and monitoring
- Maintains backward compatibility with existing torrent generation

### Files Changed

- `src/main.rs`: Added `git_lfs_clone` module and updated `repo_info` function
- `Cargo.toml`: Added `tokio` process feature
- `README.md`: Updated documentation with technical improvements
- `CHANGELOG.md`: Added comprehensive change documentation

### Future Improvements

- [x] ~~Add HF token authentication support for private repositories~~ ✅ **COMPLETED**
- [x] ~~Add retry logic for failed downloads~~ ✅ **COMPLETED**
- [x] ~~Add comprehensive timeout handling~~ ✅ **COMPLETED**
- [ ] Implement concurrent LFS file downloads for better performance
- [ ] Optimize for very large repositories (>10GB)
- [ ] Add resume capability for partially downloaded files
- [ ] Implement bandwidth throttling for large downloads 
