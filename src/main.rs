use std::ffi::{c_char, c_void, CStr};
use std::mem::MaybeUninit;
use std::sync::atomic::AtomicBool;
use std::time::SystemTime;

use libloading::Library;

const LIB_PATH_KPERF: &str = "/System/Library/PrivateFrameworks/kperf.framework/kperf";
const LIB_PATH_KPERFDATA: &str = "/System/Library/PrivateFrameworks/kperfdata.framework/kperfdata";

fn main() {
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

    collector.start();

    let mut v = LIB_PATH_KPERF.as_bytes().to_vec();
    v.sort();

    collector.end();

    dbg!(collector.count);
}

#[derive(Debug, Default, Clone, Copy)]
struct EventCount {
    elapsed: core::time::Duration,
    event_counts: [u64; 5],
}

impl EventCount {
    const fn cycles(self) -> u64 {
        self.event_counts[0]
    }
}

struct EventCollector {
    count: EventCount,
    start_clock: std::time::SystemTime,

    // apple-specific
    apple_events: AppleEvents,
    diff: PerformanceCounters,

    kperf: &'static Library,
    kperf_symbols: KperfSymbols<'static>,
}

impl EventCollector {
    fn new(kperf: Library, kperfdata: Library) -> Self {
        let kperf = Box::leak(Box::new(kperf));
        let kperf_symbols = unsafe { KperfSymbols::load(kperf).unwrap() };

        let mut apple_events = AppleEvents::new();
        apple_events.setup_performance_counters();

        Self {
            count: EventCount::default(),
            start_clock: SystemTime::now(),
            apple_events,
            diff: PerformanceCounters::default(),

            kperf,
            kperf_symbols,
        }
    }

    fn has_events(&mut self) -> bool {
        self.apple_events.setup_performance_counters()
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

    fn from_value(init: f64) -> Self {
        Self {
            cycles: init,
            branches: init,
            missed_branches: init,
            instructions: init,
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

    fn min_assign(&mut self, other: &Self) {
        self.cycles = self.cycles.min(other.cycles);
        self.branches = self.branches.min(other.branches);
        self.missed_branches = self.missed_branches.min(other.missed_branches);
        self.instructions = self.instructions.min(other.instructions);
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

struct AppleEvents {
    regs: [u32; KPC_MAX_COUNTERS],
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

    fn setup_performance_counters(&mut self) -> bool {
        if self.init {
            return self.worked;
        }
        self.init = true;

        let kperf = match unsafe { libloading::Library::new(LIB_PATH_KPERF) } {
            Ok(lib) => lib,
            Err(e) => {
                eprintln!("Error loading {LIB_PATH_KPERF}: {:?}", e);
                self.worked = false;
                return false;
            }
        };

        let kperfdata = match unsafe { libloading::Library::new(LIB_PATH_KPERFDATA) } {
            Ok(lib) => lib,
            Err(e) => {
                eprintln!("Error loading {LIB_PATH_KPERFDATA}: {:?}", e);
                self.worked = false;
                return false;
            }
        };

        let kperf_symbols = unsafe { KperfSymbols::load(&kperf) }.unwrap();
        let kperfdata_symbols = unsafe { KperfDataSymbols::load(&kperfdata) }.unwrap();

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

        todo!()
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
        pub struct $struct_name<'a> {
            $( pub $field_name: libloading::Symbol<'a, unsafe extern fn( $( $arg ),* ) -> $ret>, )*
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
    kpep_config_create: fn() -> u64,
    kpep_config_free: fn() -> u64,
    kpep_config_add_event: fn() -> u64,
    kpep_config_remove_event: fn() -> u64,
    kpep_config_force_counters: fn() -> u64,
    kpep_config_events_count: fn() -> u64,
    kpep_config_events: fn() -> u64,
    kpep_config_kpc: fn() -> u64,
    kpep_config_kpc_count: fn() -> u64,
    kpep_config_kpc_classes: fn() -> u64,
    kpep_config_kpc_map: fn() -> u64,
    kpep_db_create: fn(*const c_char, *mut *mut kpep_db) -> u64,
    kpep_db_free: fn() -> u64,
    kpep_db_name: fn() -> u64,
    kpep_db_aliases_count: fn() -> u64,
    kpep_db_aliases: fn() -> u64,
    kpep_db_counters_count: fn() -> u64,
    kpep_db_events_count: fn() -> u64,
    kpep_db_events: fn() -> u64,
    kpep_db_event: fn() -> u64,
    kpep_event_name: fn() -> u64,
    kpep_event_alias: fn() -> u64,
    kpep_event_description: fn() -> u64,
);
