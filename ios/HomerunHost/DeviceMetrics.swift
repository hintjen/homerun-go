import Foundation

/// Process-level memory and CPU figures.
///
/// The server runs *inside* this process, so there is no per-server number to
/// report — these describe the app as a whole, which while a world is up is
/// dominated by the server anyway.
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

/// Turns cumulative CPU time into a percentage.
///
/// CPU usage is a rate, so it only exists between two samples — the first call
/// has nothing to compare against and reports nothing rather than inventing a
/// number.
final class CPUSampler {
    private var lastCPUSeconds: Double?
    private var lastSampledAt: Date?

    func sample() -> Double? {
        guard let cpu = DeviceMetrics.cpuSeconds() else { return nil }
        let now = Date()

        defer {
            lastCPUSeconds = cpu
            lastSampledAt = now
        }

        guard let previousCPU = lastCPUSeconds, let previousAt = lastSampledAt else { return nil }
        let elapsed = now.timeIntervalSince(previousAt)
        guard elapsed > 0 else { return nil }

        // Can exceed 100% — several cores, and the server uses them.
        return ((cpu - previousCPU) / elapsed) * 100.0
    }
}
