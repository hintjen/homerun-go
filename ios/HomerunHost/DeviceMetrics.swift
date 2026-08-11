import Foundation

/// Process-level memory and CPU **counters**.
///
/// The server runs *inside* this process, so there is no per-server number to
/// report — these describe the app as a whole, which while a world is up is
/// dominated by the server anyway.
///
/// Counters only: nothing here computes a rate. A percentage is a difference
/// between two moments, and which two is a decision `homerun-core::metrics`
/// owns for every platform at once — the same split `ProcMetrics.kt` opens
/// with on Android. This file's job is to read the numbers the OS keeps and
/// hand them over unchanged.
enum DeviceMetrics {

    /// Physical footprint in KB: the number iOS uses when deciding what to
    /// jetsam, and the one Xcode's memory gauge shows.
    static func footprintKb() -> Int? {
        var info = task_vm_info_data_t()
        var count = mach_msg_type_number_t(
            MemoryLayout<task_vm_info_data_t>.size / MemoryLayout<integer_t>.size)

        let result = withUnsafeMutablePointer(to: &info) {
            $0.withMemoryRebound(to: integer_t.self, capacity: Int(count)) {
                task_info(mach_task_self_, task_flavor_t(TASK_VM_INFO), $0, &count)
            }
        }

        guard result == KERN_SUCCESS else { return nil }
        return Int(info.phys_footprint / 1024)
    }

    /// The ceiling `footprintKb` is measured against: the dirty-memory limit
    /// this app is killed for exceeding, in KB.
    ///
    /// Not the device's RAM, which is what this used to report. A phone with
    /// 16 GB does not let one app have 16 GB, so "67 MB of 16,384 MB" told a
    /// player they had room they were never going to get. This is the same
    /// question Android answers with `largeMemoryClass` — how much may I use
    /// before I am killed — so the two platforms' gauges finally mean the same
    /// thing.
    ///
    /// `os_proc_available_memory` reports what is *left*, so the limit is that
    /// plus what is already used. iOS-only by declaration; a Mac has no jetsam
    /// limit to report, which is why `ios/coretest` compiles this file without
    /// it.
    static func memoryLimitKb() -> Int? {
        #if os(iOS)
            // Zero means the caller is not an app, or is already **over** its
            // limit. Neither is a ceiling to draw a bar against, and a made-up
            // one would be worse than none — the UI simply omits the "of X"
            // when this is absent.
            //
            // The **simulator** always answers zero: it is a macOS process
            // with no jetsam limit, so there is genuinely no cap to report
            // there. Measured — `footprintKb` reads normally beside it.
            let remaining = os_proc_available_memory()
            guard remaining > 0, let used = footprintKb() else { return nil }
            return used + Int(remaining / 1024)
        #else
            return nil
        #endif
    }

    /// Total CPU time consumed by every thread in the process, in seconds.
    static func cpuSeconds() -> Double? {
        var threads: thread_act_array_t?
        var count: mach_msg_type_number_t = 0
        guard task_threads(mach_task_self_, &threads, &count) == KERN_SUCCESS,
            let threads
        else { return nil }

        defer {
            // The kernel allocated this array; leaking it every few seconds
            // would be a slow but real drain.
            for index in 0..<Int(count) {
                mach_port_deallocate(mach_task_self_, threads[index])
            }
            vm_deallocate(
                mach_task_self_, vm_address_t(UInt(bitPattern: threads)),
                vm_size_t(Int(count) * MemoryLayout<thread_t>.size))
        }

        var total = 0.0
        for index in 0..<Int(count) {
            var info = thread_basic_info()
            // THREAD_BASIC_INFO_COUNT is a C macro and does not reach Swift;
            // it is just the struct's size in `integer_t` units.
            var infoCount = mach_msg_type_number_t(
                MemoryLayout<thread_basic_info_data_t>.size / MemoryLayout<integer_t>.size)

            let result = withUnsafeMutablePointer(to: &info) {
                $0.withMemoryRebound(to: integer_t.self, capacity: Int(infoCount)) {
                    thread_info(threads[index], thread_flavor_t(THREAD_BASIC_INFO), $0, &infoCount)
                }
            }
            guard result == KERN_SUCCESS else { continue }
            if info.flags & TH_FLAGS_IDLE != 0 { continue }

            total += Double(info.user_time.seconds) + Double(info.user_time.microseconds) / 1e6
            total += Double(info.system_time.seconds) + Double(info.system_time.microseconds) / 1e6
        }
        return total
    }
}
