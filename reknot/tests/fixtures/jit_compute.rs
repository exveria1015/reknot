#[cfg(bench_version)]
const VERSION: u64 = include!(env!("REKNOT_BENCH_VERSION_FILE"));
#[cfg(not(bench_version))]
const VERSION: u64 = if cfg!(version_two) { 3 } else { 1 };

#[inline(never)]
fn mix(mut value: u64) -> u64 {
    for i in 0..64u64 {
        value = value
            .wrapping_mul(6364136223846793005)
            .wrapping_add(i + VERSION);
        value = value.rotate_left(7);
    }
    value
}

#[no_mangle]
#[inline(never)]
pub extern "C" fn bench(value: u64, callback: Option<extern "C" fn(u64)>) -> u64 {
    let result = mix(value);
    if let Some(callback) = callback {
        callback(result);
    }
    result
}
