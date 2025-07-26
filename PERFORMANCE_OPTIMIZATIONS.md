# Performance Optimizations for cuenv

## Analysis Summary

After analyzing the codebase, I've identified several performance bottlenecks and opportunities for optimization:

### 1. Excessive Use of `Mutex` Where `RwLock` Would Be Better

**Locations:**

- `src/sync_env.rs`: Global ENV_MUTEX - mostly used for reading
- `src/output_filter.rs`: Secrets HashSet - read heavy during filtering
- `src/hook_manager.rs`: LRU cache - read heavy for cache hits
- `src/secrets.rs`: approval_shown flag - read heavy
- `src/cleanup.rs`: CLEANUP_REGISTRY - read heavy for checking resources

### 2. Unnecessary Cloning

**Major Issues:**

- Task configurations cloned for each task execution
- Environment variable maps cloned multiple times
- Command arguments cloned unnecessarily
- String allocations in hot paths

### 3. Missing Capacity Hints

**Locations:**

- HashMaps and Vecs created without capacity hints despite known sizes
- Task dependency collections
- Environment variable collections
- Command output buffers

### 4. String Allocation Issues

**Problems:**

- Excessive `to_string()` calls
- String concatenation in loops
- Repeated string formatting

## Implementation Plan

### Phase 1: Replace Mutex with RwLock (High Priority)

1. **sync_env.rs** - Convert ENV_MUTEX to RwLock
2. **output_filter.rs** - Convert secrets Mutex to RwLock
3. **hook_manager.rs** - Convert cache Mutex to RwLock
4. **cleanup.rs** - Convert CLEANUP_REGISTRY to RwLock

### Phase 2: Reduce Cloning (High Priority)

1. Use references and borrowing where possible
2. Implement `Arc` for shared immutable data
3. Use `Cow` (Clone-on-Write) for conditionally modified strings
4. Pass references to task configurations instead of cloning

### Phase 3: Add Capacity Hints (Medium Priority)

1. Pre-allocate collections based on known sizes
2. Use `with_capacity` for Vec and HashMap creation
3. Reserve capacity before bulk insertions

### Phase 4: Consider parking_lot (Low Priority)

1. Benchmark standard library vs parking_lot
2. Replace if significant performance gains

## Next Steps

I'll start implementing these optimizations in order of priority, beginning with the RwLock conversions and clone reduction.
