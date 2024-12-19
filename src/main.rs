use std::ffi::{c_char, c_void, CStr};
use std::sync::atomic::AtomicBool;
use std::time::SystemTime;

use libloading::Library;

const LIB_PATH_KPERF: &str = "/System/Library/PrivateFrameworks/kperf.framework/kperf";
const LIB_PATH_KPERFDATA: &str = "/System/Library/PrivateFrameworks/kperfdata.framework/kperfdata";

fn main() {
    dbg!(count_events(100, || {
        let mut v = LIB_PATH_KPERF.as_bytes().to_vec();
        v.sort();
    }));
}

#[derive(Debug)]
struct Run {
    mean: PerformanceCounters,
    minimum: PerformanceCounters,
    maximum: PerformanceCounters,
    standard_deviation: PerformanceCounters,
}

fn count_events(repeat: usize, f: impl Fn() -> ()) -> Run {
    let kperf = match unsafe { libloading::Library::new(LIB_PATH_KPERF) } {
        Ok(lib) => lib,
        Err(e) => {
            panic!("Error loading {LIB_PATH_KPERF}: {:?}", e)
        }
    };

    let kperfdata = match unsafe { libloading::Library::new(LIB_PATH_KPERFDATA) } {
        Ok(lib) => lib,
        Err(e) => {
            panic!("Error loading {LIB_PATH_KPERFDATA}: {:?}", e)
        }
    };

    let mut collector = EventCollector::new(kperf, kperfdata);

    let mut samples = Vec::with_capacity(repeat);

    for _ in 0..repeat {
        collector.start();

        f();

        samples.push(collector.end());
    }

    let mut total = PerformanceCounters::default();
    let mut minimum = PerformanceCounters::from_value(1e300);
    let mut maximum = PerformanceCounters::from_value(0.0);

    for sample in samples.iter() {
        let sample = PerformanceCounters::from_event_count(*sample);
        total += sample;
        minimum.min(&sample);
        maximum.max(&sample);
    }

    let mut mean = total;
    mean /= repeat as f64;

    let mut variance = PerformanceCounters::default();

    for sample in samples.iter() {
        let sample = PerformanceCounters::from_event_count(*sample);
        let diff = sample - mean;
        variance += diff.squared();
    }

    Run {
        mean,
        minimum,
        maximum,
        standard_deviation: variance.sqrt(),
    }
}

#[derive(Default, Clone, Copy)]
struct EventCount {
    elapsed: core::time::Duration,
    event_counts: [u64; 5],
}

impl std::fmt::Debug for EventCount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventCount")
            .field("elapsed", &self.elapsed)
            .field("event_counts", &self.event_counts)
            .field("cycles", &self.cycles())
            .field("instructions", &self.instructions())
            .field("missed_branches", &self.missed_branches())
            .field("branches", &self.branches())
            .finish()
    }
}

impl EventCount {
    const fn cycles(self) -> u64 {
        self.event_counts[0]
    }

    const fn instructions(self) -> u64 {
        self.event_counts[1]
    }

    const fn missed_branches(self) -> u64 {
        self.event_counts[2]
    }

    const fn branches(self) -> u64 {
        self.event_counts[4]
    }
}

struct EventCollector {
    count: EventCount,
    start_clock: std::time::SystemTime,

    // apple-specific
    apple_events: AppleEvents,
    diff: PerformanceCounters,

    // kept around so that they can be dropped at the end
    kperf: Option<&'static Library>,
    kperfdata: Option<&'static Library>,

    kperf_symbols: KperfSymbols<'static>,
    kperfdata_symbols: KperfDataSymbols<'static>,
}

impl Drop for EventCollector {
    fn drop(&mut self) {
        if let Some(library) = self.kperf.take() {
            let _ = unsafe { Box::from_raw(library as *const Library as *mut Library) };
        }

        if let Some(library) = self.kperfdata.take() {
            let _ = unsafe { Box::from_raw(library as *const Library as *mut Library) };
        }
    }
}

impl EventCollector {
    fn new(kperf: Library, kperfdata: Library) -> Self {
        let kperf = Box::leak(Box::new(kperf));
        let kperf_symbols = unsafe { KperfSymbols::load(kperf).unwrap() };

        let kperfdata = Box::leak(Box::new(kperfdata));
        let kperfdata_symbols = unsafe { KperfDataSymbols::load(kperfdata).unwrap() };

        let mut apple_events = AppleEvents::new();
        apple_events.setup_performance_counters(&kperf_symbols, &kperfdata_symbols);

        Self {
            count: EventCount::default(),
            start_clock: SystemTime::now(),
            apple_events,
            diff: PerformanceCounters::default(),

            kperf: Some(kperf),
            kperf_symbols,

            kperfdata: Some(kperfdata),
            kperfdata_symbols,
        }
    }

    fn has_events(&mut self) -> bool {
        self.apple_events
            .setup_performance_counters(&self.kperf_symbols, &self.kperfdata_symbols)
    }

    #[inline(always)]
    fn start(&mut self) {
        if self.has_events() {
            self.diff = self.apple_events.get_counters(&self.kperf_symbols);
        }
    }

    #[inline(always)]
    fn end(&mut self) -> EventCount {
        let end_clock = std::time::SystemTime::now();

        if self.has_events() {
            let end = self.apple_events.get_counters(&self.kperf_symbols);
            self.diff = end - self.diff;
        }

        self.count.event_counts[0] = self.diff.cycles as u64;
        self.count.event_counts[1] = self.diff.instructions as u64;
        self.count.event_counts[2] = self.diff.missed_branches as u64;
        self.count.event_counts[3] = 0 as u64;
        self.count.event_counts[4] = self.diff.branches as u64;

        self.count.elapsed = end_clock.duration_since(self.start_clock).unwrap();

        return self.count;
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct PerformanceCounters {
    cycles: f64,
    branches: f64,
    missed_branches: f64,
    instructions: f64,
}

impl PerformanceCounters {
    // Constructors
    fn new_u64(c: u64, b: u64, m: u64, i: u64) -> Self {
        Self {
            cycles: c as f64,
            branches: b as f64,
            missed_branches: m as f64,
            instructions: i as f64,
        }
    }

    fn new_f64(c: f64, b: f64, m: f64, i: f64) -> Self {
        Self {
            cycles: c,
            branches: b,
            missed_branches: m,
            instructions: i,
        }
    }

    fn from_event_count(event_count: EventCount) -> Self {
        Self {
            cycles: event_count.cycles() as f64,
            branches: event_count.branches() as f64,
            missed_branches: event_count.missed_branches() as f64,
            instructions: event_count.instructions() as f64,
        }
    }

    fn from_value(init: f64) -> Self {
        Self {
            cycles: init,
            branches: init,
            missed_branches: init,
            instructions: init,
        }
    }

    fn squared(self) -> Self {
        Self {
            cycles: self.cycles * self.cycles,
            branches: self.branches * self.branches,
            missed_branches: self.missed_branches * self.missed_branches,
            instructions: self.instructions * self.instructions,
        }
    }

    fn sqrt(self) -> Self {
        Self {
            cycles: self.cycles.sqrt(),
            branches: self.branches.sqrt(),
            missed_branches: self.missed_branches.sqrt(),
            instructions: self.instructions.sqrt(),
        }
    }

    // Methods for in-place operations
    fn subtract_assign(&mut self, other: &Self) {
        self.cycles -= other.cycles;
        self.branches -= other.branches;
        self.missed_branches -= other.missed_branches;
        self.instructions -= other.instructions;
    }

    fn add_assign(&mut self, other: &Self) {
        self.cycles += other.cycles;
        self.branches += other.branches;
        self.missed_branches += other.missed_branches;
        self.instructions += other.instructions;
    }

    fn divide_assign(&mut self, numerator: f64) {
        self.cycles /= numerator;
        self.branches /= numerator;
        self.missed_branches /= numerator;
        self.instructions /= numerator;
    }

    fn min(&mut self, other: &Self) {
        self.cycles = f64::min(self.cycles, other.cycles);
        self.branches = f64::min(self.branches, other.branches);
        self.missed_branches = f64::min(self.missed_branches, other.missed_branches);
        self.instructions = f64::min(self.instructions, other.instructions);
    }

    fn max(&mut self, other: &Self) {
        self.cycles = f64::max(self.cycles, other.cycles);
        self.branches = f64::max(self.branches, other.branches);
        self.missed_branches = f64::max(self.missed_branches, other.missed_branches);
        self.instructions = f64::max(self.instructions, other.instructions);
    }
}

// Operator overloads as standalone functions
impl std::ops::Sub for PerformanceCounters {
    type Output = Self;

    fn sub(self, other: Self) -> Self::Output {
        Self {
            cycles: self.cycles - other.cycles,
            branches: self.branches - other.branches,
            missed_branches: self.missed_branches - other.missed_branches,
            instructions: self.instructions - other.instructions,
        }
    }
}

impl std::ops::SubAssign for PerformanceCounters {
    fn sub_assign(&mut self, other: Self) {
        self.subtract_assign(&other);
    }
}

impl std::ops::AddAssign for PerformanceCounters {
    fn add_assign(&mut self, other: Self) {
        self.add_assign(&other);
    }
}

impl std::ops::DivAssign<f64> for PerformanceCounters {
    fn div_assign(&mut self, numerator: f64) {
        self.divide_assign(numerator);
    }
}

/// The maximum number of counters we could read from every class in one go.
/// ARMV7: FIXED: 1, CONFIGURABLE: 4
/// ARM32: FIXED: 2, CONFIGURABLE: 6
/// ARM64: FIXED: 2, CONFIGURABLE: CORE_NCTRS - FIXED (6 or 8)
/// x86: 32
const KPC_MAX_COUNTERS: usize = 32;

/// KPEP event (size: 48/28 bytes on 64/32 bit OS)
struct kpep_event {
    ///< Unique name of a event, such as "INST_RETIRED.ANY".
    name: *const c_char,
    ///< Description for this event.
    description: *const c_char,
    ///< Errata, currently NULL.
    errata: *const c_char,
    ///< Alias name, such as "Instructions", "Cycles".
    alias: *const c_char,
    ///< Fallback event name for fixed counter.
    fallback: *const c_char,
    mask: u32,
    number: u8,
    umask: u8,
    reserved: u8,
    is_fixed: u8,
}

struct kpep_config {
    db: *mut kpep_db,
    ///< (sizeof(kpep_event *) * counter_count), init NULL
    ///< (sizeof(usize *) * counter_count), init 0
    ev_map: *mut usize,
    ///< (sizeof(usize *) * counter_count), init -1
    ev_idx: *mut usize,
    ///< (sizeof(u32 *) * counter_count), init 0
    flags: *mut i32,
    ///< (sizeof(u64 *) * counter_count), init 0
    kpc_periods: *mut u64,
    /// kpep_config_events_count()
    event_count: usize,
    counter_count: usize,
    classes: u32,
    ///< See `class mask constants` above.
    config_counter: u32,
    power_counter: u32,
    reserved: u32,
}

const EVENT_NAME_MAX: usize = 8;

struct event_alias {
    /// name for print
    alias: *const c_char,
    /// name from pmc db
    names: [*const c_char; EVENT_NAME_MAX],
}

/// Event names from /usr/share/kpep/<name>.plist
const profile_events: [event_alias; 4] = [
    event_alias {
        alias: c"cycles".as_ptr(),
        names: [
            c"FIXED_CYCLES".as_ptr(),            // Apple A7-A15
            c"CPU_CLK_UNHALTED.THREAD".as_ptr(), // Intel Core 1th-10th
            c"CPU_CLK_UNHALTED.CORE".as_ptr(),   // Intel Yonah, Merom
            core::ptr::null(),
            core::ptr::null(),
            core::ptr::null(),
            core::ptr::null(),
            core::ptr::null(),
        ],
    },
    event_alias {
        alias: c"instructions".as_ptr(),
        names: [
            c"FIXED_INSTRUCTIONS".as_ptr(), // Apple A7-A15
            c"INST_RETIRED.ANY".as_ptr(),   // Intel Yonah, Merom, Core 1th-10th
            core::ptr::null(),
            core::ptr::null(),
            core::ptr::null(),
            core::ptr::null(),
            core::ptr::null(),
            core::ptr::null(),
        ],
    },
    event_alias {
        alias: c"branches".as_ptr(),
        names: [
            c"INST_BRANCH".as_ptr(),                  // Apple A7-A15
            c"BR_INST_RETIRED.ALL_BRANCHES".as_ptr(), // Intel Core 1th-10th
            c"INST_RETIRED.ANY".as_ptr(),             // Intel Yonah, Merom
            core::ptr::null(),
            core::ptr::null(),
            core::ptr::null(),
            core::ptr::null(),
            core::ptr::null(),
        ],
    },
    event_alias {
        alias: c"branch-misses".as_ptr(),
        names: [
            c"BRANCH_MISPRED_NONSPEC".as_ptr(), // Apple A7-A15, since iOS 15, macOS 12
            c"BRANCH_MISPREDICT".as_ptr(),      // Apple A7-A14
            c"BR_MISP_RETIRED.ALL_BRANCHES".as_ptr(), // Intel Core 2th-10th
            c"BR_INST_RETIRED.MISPRED".as_ptr(), // Intel Yonah, Merom
            core::ptr::null(),
            core::ptr::null(),
            core::ptr::null(),
            core::ptr::null(),
        ],
    },
];

unsafe fn get_event(
    kperfdata: &KperfDataSymbols,
    db: *mut kpep_db,
    alias: &event_alias,
) -> *mut kpep_event {
    for name in alias.names {
        if name.is_null() {
            break;
        }

        let mut ev = core::ptr::null_mut();
        if (kperfdata.kpep_db_event)(db, name, &mut ev) == 0 {
            return ev;
        }
    }

    return core::ptr::null_mut();
}

struct AppleEvents {
    regs: [u64; KPC_MAX_COUNTERS],
    counter_map: [usize; KPC_MAX_COUNTERS],
    counters_0: [u64; KPC_MAX_COUNTERS],
    counters_1: [u64; KPC_MAX_COUNTERS],
    init: bool,
    worked: bool,
}

impl AppleEvents {
    fn new() -> Self {
        Self {
            regs: [0; KPC_MAX_COUNTERS],
            counter_map: [0; KPC_MAX_COUNTERS],
            counters_0: [0; KPC_MAX_COUNTERS],
            counters_1: [0; KPC_MAX_COUNTERS],
            init: false,
            worked: false,
        }
    }

    fn setup_performance_counters(
        &mut self,
        kperf_symbols: &KperfSymbols,
        kperfdata_symbols: &KperfDataSymbols,
    ) -> bool {
        if self.init {
            return self.worked;
        }
        self.init = true;

        // Check permission
        let mut force_ctrs = 0;
        if unsafe { (kperf_symbols.kpc_force_all_ctrs_get)(&mut force_ctrs) } != 0 {
            println!("Permission denied, xnu/kpc requires root privileges.");
            self.worked = false;
            return false;
        }

        // Load PMC database
        let mut db: *mut kpep_db = core::ptr::null_mut();
        match unsafe { (kperfdata_symbols.kpep_db_create)(core::ptr::null_mut(), &mut db) } {
            0 => { /* all good */ }
            ret => {
                println!("Error: cannot load pmc database: {}.", ret);
                self.worked = false;
                return false;
            }
        };

        let name = unsafe { CStr::from_ptr((*db).name).to_string_lossy() };
        let marketing_name = unsafe { CStr::from_ptr((*db).marketing_name).to_string_lossy() };
        println!("Loaded db: {} ({})", name, marketing_name);

        // create a config
        let mut cfg: *mut kpep_config = core::ptr::null_mut();
        match unsafe { (kperfdata_symbols.kpep_config_create)(db, &mut cfg) } {
            0 => {}
            ret => {
                // eprintln!( "Failed to create kpep config: %d (%s).\n", ret, kpep_config_error_desc(ret),);
                eprintln!("Failed to create kpep config");
                self.worked = false;
                return self.worked;
            }
        }

        match unsafe { (kperfdata_symbols.kpep_config_force_counters)(cfg) } {
            0 => {}
            ret => {
                // printf( "Failed to force counters: %d (%s).\n", ret, kpep_config_error_desc(ret),);
                eprintln!("Failed to force counters");
                self.worked = false;
                return self.worked;
            }
        }

        // get events
        let mut ev_arr: [*mut kpep_event; profile_events.len()] =
            [core::ptr::null_mut(); profile_events.len()];
        for (i, alias) in profile_events.iter().enumerate() {
            ev_arr[i] = unsafe { get_event(&kperfdata_symbols, db, alias) };
            if ev_arr[i].is_null() {
                // printf("Cannot find event: %s.\n", alias->alias);
                eprintln!("Cannot find event");
                self.worked = false;
                return self.worked;
            }
        }

        // add event to config
        for ev in ev_arr.iter_mut() {
            match unsafe {
                (kperfdata_symbols.kpep_config_add_event)(cfg, ev, 0, core::ptr::null_mut())
            } {
                0 => {}
                ret => {
                    // printf( "Failed to force counters: %d (%s).\n", ret, kpep_config_error_desc(ret),);
                    eprintln!("Failed to force counters");
                    self.worked = false;
                    return self.worked;
                }
            }
        }

        // prepare buffer and config
        let mut classes: u32 = 0;
        let mut reg_count: usize = 0;
        match unsafe { (kperfdata_symbols.kpep_config_kpc_classes)(cfg, &mut classes) } {
            0 => {}
            ret => {
                // printf("Failed get kpc classes: %d (%s).\n", ret, kpep_config_error_desc(ret));
                eprintln!("error");
                self.worked = false;
                return self.worked;
            }
        }
        match unsafe { (kperfdata_symbols.kpep_config_kpc_count)(cfg, &mut reg_count) } {
            0 => {}
            ret => {
                // printf("Failed get kpc count: %d (%s).\n", ret, kpep_config_error_desc(ret));
                eprintln!("error");
                self.worked = false;
                return self.worked;
            }
        }
        match unsafe {
            (kperfdata_symbols.kpep_config_kpc_map)(
                cfg,
                self.counter_map.as_mut_ptr(),
                core::mem::size_of_val(&self.counter_map),
            )
        } {
            0 => {}
            ret => {
                // printf("Failed get kpc map: %d (%s).\n", ret, kpep_config_error_desc(ret));

                eprintln!("error");
                self.worked = false;
                return self.worked;
            }
        }
        match unsafe {
            (kperfdata_symbols.kpep_config_kpc)(
                cfg,
                self.regs.as_mut_ptr(),
                core::mem::size_of_val(&self.regs),
            )
        } {
            0 => {}
            ret => {
                // printf("Failed get kpc registers: %d (%s).\n", ret, kpep_config_error_desc(ret));
                eprintln!("error");
                self.worked = false;
                return self.worked;
            }
        }

        // set config to kernel
        match unsafe { (kperf_symbols.kpc_force_all_ctrs_set)(1) } {
            0 => {}
            ret => {
                eprintln!("Failed force all ctrs: {ret}");
                self.worked = false;
                return self.worked;
            }
        }
        if (classes & KPC_CLASS_CONFIGURABLE_MASK as u32) != 0 && reg_count != 0 {
            match unsafe { (kperf_symbols.kpc_set_config)(classes, self.regs.as_ptr()) } {
                0 => {}
                ret => {
                    eprintln!("Failed set kpc config: {ret}");
                    self.worked = false;
                    return self.worked;
                }
            }
        }

        // start counting
        match unsafe { (kperf_symbols.kpc_set_counting)(classes) } {
            0 => {}
            ret => {
                eprintln!("Failed set counting: {ret}");
                self.worked = false;
                return self.worked;
            }
        }
        match unsafe { (kperf_symbols.kpc_set_thread_counting)(classes) } {
            0 => {}
            ret => {
                eprintln!("Failed set thread counting: {ret}");
                self.worked = false;
                return self.worked;
            }
        }

        self.worked = true;
        self.worked
    }

    fn get_counters(&mut self, kperf: &KperfSymbols) -> PerformanceCounters {
        static WARNED: AtomicBool = AtomicBool::new(false);
        if unsafe {
            (kperf.kpc_get_thread_counters)(
                0,
                KPC_MAX_COUNTERS as u32,
                self.counters_0.as_mut_ptr(),
            )
        } != 0
        {
            if !WARNED.fetch_or(true, std::sync::atomic::Ordering::Relaxed) {
                println!("Failed to get thread counters.");
            }

            return PerformanceCounters::from_value(1.0);
        }

        PerformanceCounters::new_f64(
            self.counters_0[self.counter_map[0]] as f64,
            self.counters_0[self.counter_map[2]] as f64,
            self.counters_0[self.counter_map[3]] as f64,
            self.counters_0[self.counter_map[1]] as f64,
        )
    }
}

/// KPEP database (size: 144/80 bytes on 64/32 bit OS)
#[derive(Debug)]
struct kpep_db {
    ///< Database name, such as "haswell".
    name: *const c_char,
    ///< Plist name, such as "cpu_7_8_10b282dc".
    cpu_id: *const c_char,
    ///< Marketing name, such as "Intel Haswell".
    marketing_name: *const c_char,
    ///< Plist data (CFDataRef), currently NULL.
    plist_data: *mut c_void,
    ///< All events (CFDict<CFSTR(event_name), kpep_event *>).
    event_map: *mut c_void,
    ///< Event struct buffer (sizeof(kpep_event) * events_count).
    event_arr: *mut kpep_event,
    ///< Fixed counter events (sizeof(kpep_event *)
    fixed_event_arr: *mut *mut kpep_event,

    ///< All aliases (CFDict<CFSTR(event_name), kpep_event *>).             ///< * fixed_counter_count)
    alias_map: *mut c_void,
    reserved_1: usize,
    reserved_2: usize,
    reserved_3: usize,
    ///< All events count
    event_count: usize,
    alias_count: usize,
    fixed_counter_count: usize,
    config_counter_count: usize,
    power_counter_count: usize,
    ///< see `KPEP CPU archtecture constants` above
    archtecture: u32,
    fixed_counter_bits: u32,
    config_counter_bits: u32,
    power_counter_bits: u32,
}

macro_rules! load_dynlib_symbols {
    ( $struct_name:ident ; $( $field_name:ident : fn( $( $arg:ty ),* ) -> $ret:ty ),* $(,)? ) => {
        #[allow(dead_code)]
        pub struct $struct_name<'a> {
            $( $field_name: libloading::Symbol<'a, unsafe extern fn( $( $arg ),* ) -> $ret>, )*
        }

        impl<'a> $struct_name<'a> {
            pub unsafe fn load(lib: &'a libloading::Library) -> Result<Self, Box<dyn std::error::Error>> {
                Ok($struct_name {
                    $( $field_name: lib.get::<unsafe extern fn( $( $arg ),* ) -> $ret>(stringify!($field_name).as_bytes())?, )*
                })
            }
        }
    };
}

load_dynlib_symbols!(
    KperfSymbols;
    kpc_pmu_version: fn() -> u32,
    kpc_cpu_string: fn(*mut char, usize) -> i32,
    kpc_set_counting: fn(u32) -> i32,
    kpc_get_counting: fn() -> u32,
    kpc_set_thread_counting: fn(u32) -> i32,
    kpc_get_thread_counting: fn() -> u32,
    kpc_get_config_count: fn(u32) -> u32,
    kpc_get_counter_count: fn(u32) -> u32,
    kpc_set_config: fn(u32, *const u64) -> i32,
    kpc_get_config: fn(u32, *mut u64) -> i32,
    kpc_get_cpu_counters: fn(bool, u32, *mut i32, *mut u64) -> i32,
    kpc_get_thread_counters: fn(u32, u32, *mut u64) -> i32,
    kpc_force_all_ctrs_set: fn(i32) -> i32,
    kpc_force_all_ctrs_get: fn(*mut i32) -> i32,
    kperf_action_count_set: fn(u32) -> i32,
    kperf_action_count_get: fn(*mut u32) -> i32,
    kperf_action_samplers_set: fn(u32, u32) -> i32,
    kperf_action_samplers_get: fn(u32, *mut u32) -> i32,
    kperf_action_filter_set_by_task: fn(u32, i32) -> i32,
    kperf_action_filter_set_by_pid: fn(u32, i32) -> i32,
    kperf_timer_count_set: fn(u32) -> i32,
    kperf_timer_count_get: fn(*mut u32) -> i32,
    kperf_timer_period_set: fn(u32, u64) -> i32,
    kperf_timer_period_get: fn(u32, *mut u64) -> i32,
    kperf_timer_action_set: fn(u32, u32) -> i32,
    kperf_timer_action_get: fn(u32, *mut u32) -> i32,
    kperf_sample_set: fn(u32) -> i32,
    kperf_sample_get: fn(*mut u32) -> i32,
    kperf_reset: fn() -> i32,
    kperf_timer_pet_set: fn(u32) -> i32,
    kperf_timer_pet_get: fn(*mut u32) -> i32,
    kperf_ns_to_ticks: fn(u64) -> u64,
    kperf_ticks_to_ns: fn(u64) -> u64,
    kperf_tick_frequency: fn() -> u64,
);

load_dynlib_symbols!(
    KperfDataSymbols;
    kpep_config_create: fn(*mut kpep_db, *mut *mut kpep_config) -> i32,
    kpep_config_free: fn(*mut kpep_config) -> (),
    kpep_config_add_event: fn(*mut kpep_config, *mut *mut kpep_event, u32, *mut u32) -> i32,
    kpep_config_remove_event: fn(*mut kpep_config, usize) -> i32,
    kpep_config_force_counters: fn(*mut kpep_config) -> i32,
    kpep_config_events_count: fn(*mut kpep_config, *mut usize) -> i32,
    kpep_config_events: fn(*mut kpep_config, *mut *mut kpep_event, usize) -> i32,
    kpep_config_kpc: fn(*mut kpep_config, *mut u64, usize) -> i32,
    kpep_config_kpc_count: fn(*mut kpep_config, *mut usize) -> i32,
    kpep_config_kpc_classes: fn(*mut kpep_config, *mut u32) -> i32,
    kpep_config_kpc_map: fn(*mut kpep_config, *mut usize, usize) -> i32,
    kpep_db_create: fn(*const c_char, *mut *mut kpep_db) -> i32,
    kpep_db_free: fn(*mut kpep_db) -> (),
    kpep_db_name: fn(*mut kpep_db, *mut *const c_char) -> i32,
    kpep_db_aliases_count: fn(*mut kpep_db, *mut usize) -> i32,
    kpep_db_aliases: fn(*mut kpep_db, *mut *const c_char, usize) -> i32,
    kpep_db_counters_count: fn(*mut kpep_db, u8, *mut usize) -> i32,
    kpep_db_events_count: fn(*mut kpep_db, *mut usize) -> i32,
    kpep_db_events: fn(*mut kpep_db, *mut *mut kpep_event, usize) -> i32,
    kpep_db_event: fn(*mut kpep_db, *const c_char, *mut *mut kpep_event) -> i32,
    kpep_event_name: fn(*mut kpep_event, *mut *const c_char) -> i32,
    kpep_event_alias: fn(*mut kpep_event, *mut *const c_char) -> i32,
    kpep_event_description: fn(*mut kpep_event, *mut *const c_char) -> i32,
);

// -----------------------------------------------------------------------------
// <kperf.framework> header (reverse engineered)
// This framework wraps some sysctl calls to communicate with the kpc in kernel.
// Most functions requires root privileges, or process is "blessed".
// -----------------------------------------------------------------------------

// Cross-platform class constants.
const KPC_CLASS_FIXED: usize = 0;
const KPC_CLASS_CONFIGURABLE: usize = 1;
const KPC_CLASS_POWER: usize = 2;
const KPC_CLASS_RAWPMU: usize = 3;

// Cross-platform class mask constants.
const KPC_CLASS_FIXED_MASK: usize = 1 << KPC_CLASS_FIXED; // 1
const KPC_CLASS_CONFIGURABLE_MASK: usize = 1 << KPC_CLASS_CONFIGURABLE; // 2
const KPC_CLASS_POWER_MASK: usize = 1 << KPC_CLASS_POWER; // 4
const KPC_CLASS_RAWPMU_MASK: usize = 1 << KPC_CLASS_RAWPMU; // 8
