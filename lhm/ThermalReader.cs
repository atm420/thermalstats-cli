using System;
using System.Globalization;
using LibreHardwareMonitor.Hardware;

class ThermalReader
{
    static void Main()
    {
        try
        {
            var computer = new Computer
            {
                IsCpuEnabled = true,
                IsGpuEnabled = true
            };
            computer.Open();

            double? cpuTemp = null;
            double? gpuTemp = null;
            string cpuName = null;
            string gpuName = null;

            foreach (IHardware hw in computer.Hardware)
            {
                hw.Update();

                if (hw.HardwareType == HardwareType.Cpu)
                {
                    cpuName = hw.Name;
                    cpuTemp = FindTemp(hw, "Package", "Tctl", "Tdie", "Core");

                    // Also check sub-hardware (LHM nests core sensors under CPU)
                    foreach (IHardware sub in hw.SubHardware)
                    {
                        sub.Update();
                        if (!cpuTemp.HasValue)
                        {
                            cpuTemp = FindTemp(sub, "Package", "Tctl", "Tdie", "Core");
                        }
                    }
                }

                bool isGpu = hw.HardwareType == HardwareType.GpuNvidia
                          || hw.HardwareType == HardwareType.GpuAmd
                          || hw.HardwareType == HardwareType.GpuIntel;

                if (isGpu)
                {
                    gpuName = hw.Name;
                    gpuTemp = FindTemp(hw, "Hot Spot", "Core", "GPU");

                    foreach (IHardware sub in hw.SubHardware)
                    {
                        sub.Update();
                        if (!gpuTemp.HasValue)
                        {
                            gpuTemp = FindTemp(sub, "Hot Spot", "Core", "GPU");
                        }
                    }
                }
            }

            computer.Close();

            // Diagnostic: if CPU detected but no temp, list what sensors were found
            if (cpuName != null && !cpuTemp.HasValue)
            {
                Console.Error.Write("DIAG:CPU_detected=" + cpuName + ",no_temp_sensors");
            }

            // Output JSON
            string cpuVal = cpuTemp.HasValue ? cpuTemp.Value.ToString("F1", CultureInfo.InvariantCulture) : "null";
            string gpuVal = gpuTemp.HasValue ? gpuTemp.Value.ToString("F1", CultureInfo.InvariantCulture) : "null";
            string cpuN = cpuName != null ? "\"" + cpuName.Replace("\"", "\\\"") + "\"" : "null";
            string gpuN = gpuName != null ? "\"" + gpuName.Replace("\"", "\\\"") + "\"" : "null";

            Console.Write("{\"cpu\":" + cpuVal + ",\"gpu\":" + gpuVal + ",\"cpuName\":" + cpuN + ",\"gpuName\":" + gpuN + "}");
        }
        catch (Exception ex)
        {
            Console.Error.Write("ERROR:" + ex.Message);
            Environment.ExitCode = 1;
        }
    }

    /// Find a temperature sensor matching any of the given name keywords (priority order)
    static double? FindTemp(IHardware hw, params string[] keywords)
    {
        double? fallback = null;
        foreach (string keyword in keywords)
        {
            foreach (ISensor sensor in hw.Sensors)
            {
                if (sensor.SensorType == SensorType.Temperature && sensor.Value.HasValue)
                {
                    if (sensor.Name.Contains(keyword))
                    {
                        return sensor.Value.Value;
                    }
                    if (!fallback.HasValue)
                    {
                        fallback = sensor.Value.Value;
                    }
                }
            }
        }
        return fallback;
    }
}
