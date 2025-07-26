# Buck2-Inspired Cache Implementation for cuenv

## Overview

This document describes the Buck2-inspired caching improvements implemented for cuenv. The new implementation provides lock-free concurrent access, content-addressed storage, and improved performance characteristics.

## Key Components

### 1. **Concurrent Cache** (`concurrent_cache.rs`)

- **Lock-free design**: Uses DashMap for concurrent HashMap operations without explicit locking
- **Atomic statistics**: All counters use atomic operations for thread-safe updates
- **LRU eviction**: Implements size-based eviction with LRU policy
- **Benefits**:
  - No lock contention on reads/writes
  - Better scalability with multiple threads
  - Predictable performance characteristics

### 2. **Content-Addressed Storage** (`content_addressed_store.rs`)

- **Deduplication**: Files stored by content hash, eliminating duplicates
- **Integrity**: Content verification through SHA256 hashing
- **Inline storage**: Small files (<4KB) stored inline for better performance
- **Reference counting**: Tracks usage and enables garbage collection
- **Benefits**:
  - Reduced storage requirements
  - Guaranteed content integrity
  - Efficient handling of identical outputs

### 3. **Action Cache** (`action_cache.rs`)

- **Action digests**: Unique identification of actions based on inputs/command/environment
- **In-flight deduplication**: Prevents duplicate execution of same action
- **Memoization**: Caches action results with full input/output tracking
- **Benefits**:
  - Prevents redundant computations
  - Handles concurrent requests for same action
  - Complete reproducibility

### 4. **Buck2 Cache Manager** (`buck2_cache_manager.rs`)

- **Unified interface**: Combines all caching components
- **Task tracking**: Monitors active tasks for cycle detection
- **Statistics**: Comprehensive metrics for monitoring
- **Backward compatibility**: Maintains compatibility with existing cache format

## Migration Guide

### Step 1: Update Dependencies

Add to `Cargo.toml`:

```toml
dashmap = "6.1"
parking_lot = "0.12"
crossbeam = "0.8"
```

### Step 2: Replace CacheManager Usage

Old code:

```rust
let cache_manager = CacheManager::new()?;
let cache_key = cache_manager.generate_cache_key(
    task_name,
    &task_config,
    working_dir
)?;
```

New code:

```rust
let config = Buck2CacheConfig::default();
let cache_manager = Buck2CacheManager::new(config)?;
let cache_key = cache_manager.generate_cache_key(
    task_name,
    &task_config,
    working_dir
).await?;
```

### Step 3: Update Cache Operations

Old code:

```rust
// Get cached result
let result = cache_manager.get_cached_result(
    &cache_key,
    &task_config,
    working_dir
)?;

// Save result
cache_manager.save_result(
    &cache_key,
    &task_config,
    working_dir,
    exit_code
)?;
```

New code:

```rust
// Get cached result
let result = cache_manager.get_cached_result(
    &cache_key,
    &task_config,
    working_dir
).await?;

// Save result with stdout/stderr
cache_manager.save_result(
    &cache_key,
    &task_config,
    working_dir,
    exit_code,
    Some(stdout_bytes),
    Some(stderr_bytes)
).await?;
```

## Performance Improvements

### 1. **Lock-Free Operations**

- Eliminated file locking overhead
- No blocking on cache reads
- Better CPU cache utilization

### 2. **Content Deduplication**

- Reduced disk usage for identical outputs
- Faster cache lookups through smaller index

### 3. **Concurrent Execution**

- In-flight action deduplication
- Parallel cache operations
- No serialization bottlenecks

### 4. **Memory Efficiency**

- Inline storage for small objects
- Lazy loading of cache entries
- Automatic eviction of stale entries

## Configuration Options

```rust
Buck2CacheConfig {
    // Base cache directory
    cache_dir: PathBuf,

    // Maximum cache size in bytes (0 = unlimited)
    max_cache_size: u64,

    // Maximum CAS size in bytes (0 = unlimited)
    max_cas_size: u64,

    // Inline threshold for CAS (bytes)
    cas_inline_threshold: usize,

    // Enable remote caching (future feature)
    enable_remote_cache: bool,

    // Cache TTL
    cache_ttl: Duration,
}
```

## Best Practices

1. **Use Content-Based Keys**: Let the system compute cache keys based on actual inputs
2. **Store Outputs in CAS**: Use CAS for all build outputs to enable deduplication
3. **Monitor Statistics**: Regularly check cache hit rates and adjust configuration
4. **Clean Up Regularly**: Run periodic cleanup to remove stale entries

## Future Enhancements

1. **Remote Caching**: Integration with remote cache servers
2. **Distributed CAS**: Share content-addressed storage across machines
3. **Smart Eviction**: ML-based prediction of cache entry usefulness
4. **Compression**: Automatic compression of cached artifacts
5. **Cache Warming**: Prefetch likely cache entries based on patterns

## Testing

Comprehensive tests are provided in `tests/buck2_cache_test.rs`:

- Basic cache operations
- Concurrent access patterns
- Content-addressed storage
- Cache invalidation
- Cleanup and eviction

Run tests with:

```bash
cargo test buck2_cache_tests
```

## Benchmarking

Compare performance with:

```bash
# Old implementation
cargo bench --bench critical_paths -- --save-baseline old

# New implementation
cargo bench --bench critical_paths -- --baseline old
```

## Conclusion

The Buck2-inspired cache implementation provides significant improvements in:

- **Performance**: Lock-free operations and better concurrency
- **Efficiency**: Content deduplication and smart storage
- **Reliability**: Atomic operations and integrity verification
- **Scalability**: Better handling of concurrent workloads

These improvements position cuenv for better performance in CI/CD environments and large-scale builds.
