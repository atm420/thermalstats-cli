using System;
using System.Collections.Generic;
using System.Globalization;
using System.Text;
using System.Threading;
using LibreHardwareMonitor.Hardware;

// Reads CPU and GPU temperatures through LibreHardwareMonitor and prints JSON.
//
//   ThermalReader.exe                one reading, then exit
//   ThermalReader.exe --stream 1000  one JSON line every 1000 ms until stdin closes
//
// Streaming keeps the driver and sensor tree open between readings: starting
// .NET and LHM for every sample takes seconds when the CPU is fully loaded.
//
// Written for the C# 5 compiler that ships with .NET Framework
// (%WINDIR%\Microsoft.NET\Framework64\v4.0.30319\csc.exe), so it can be
// rebuilt on any Windows machine without an SDK.
class ThermalReader
{
    static int Main(string[] args)
    {
        int interval = 0;
        if (args.Length >= 1 && args[0] == "--stream")
        {
            interval = 1000;
            if (args.Length >= 2) int.TryParse(args[1], out interval);
            if (interval < 250) interval = 250;
        }

        Computer computer = null;
        try
        {
            computer = new Computer
            {
                IsCpuEnabled = true,
                IsGpuEnabled = true
            };
            computer.Open();

            if (interval == 0)
            {
                Console.Write(Read(computer));
                return 0;
            }

            // Exit as soon as the parent closes our stdin (normal exit or crash),
            // so an orphaned reader never keeps the driver open.
            Thread watcher = new Thread(delegate()
            {
                try
                {
                    while (Console.In.Read() != -1) { }
                }
                catch (Exception) { }
                Environment.Exit(0);
            });
            watcher.IsBackground = true;
            watcher.Start();

            while (true)
            {
                Console.Out.WriteLine(Read(computer));
                Console.Out.Flush();
                Thread.Sleep(interval);
            }
        }
        catch (Exception ex)
        {
            Console.Error.Write("ERROR:" + ex.Message);
            return 1;
        }
        finally
        {
            if (computer != null)
            {
                try { computer.Close(); } catch (Exception) { }
            }
        }
    }

    static string Read(Computer computer)
    {
        ISensor cpuSensor = null;
        string cpuName = null;
        ISensor lastGpuSensor = null;
        string lastGpuName = null;
        List<string> gpus = new List<string>();

        foreach (IHardware hw in computer.Hardware)
        {
            hw.Update();

            if (hw.HardwareType == HardwareType.Cpu)
            {
                cpuName = hw.Name;
                cpuSensor = FindTemp(hw, "Package", "Tctl", "Tdie", "Core");

                // Also check sub-hardware (LHM nests core sensors under CPU)
                foreach (IHardware sub in hw.SubHardware)
                {
                    sub.Update();
                    if (cpuSensor == null)
                    {
                        cpuSensor = FindTemp(sub, "Package", "Tctl", "Tdie", "Core");
                    }
                }
            }

            bool isGpu = hw.HardwareType == HardwareType.GpuNvidia
                      || hw.HardwareType == HardwareType.GpuAmd
                      || hw.HardwareType == HardwareType.GpuIntel;

            if (isGpu)
            {
                ISensor temp = FindTemp(hw, "Hot Spot", "Core", "GPU");
                ISensor load = FindLoad(hw);

                foreach (IHardware sub in hw.SubHardware)
                {
                    sub.Update();
                    if (temp == null)
                    {
                        temp = FindTemp(sub, "Hot Spot", "Core", "GPU");
                    }
                }

                lastGpuName = hw.Name;
                lastGpuSensor = temp;

                StringBuilder gpu = new StringBuilder();
                gpu.Append("{\"name\":").Append(Str(hw.Name));
                gpu.Append(",\"type\":").Append(Str(hw.HardwareType.ToString()));
                gpu.Append(",\"temp\":").Append(Num(temp));
                gpu.Append(",\"sensor\":").Append(temp != null ? Str(temp.Name) : "null");
                gpu.Append(",\"load\":").Append(Num(load));
                gpu.Append("}");
                gpus.Add(gpu.ToString());
            }
        }

        // Diagnostic: if CPU detected but no temp, say so on stderr
        if (cpuName != null && cpuSensor == null)
        {
            Console.Error.Write("DIAG:CPU_detected=" + cpuName + ",no_temp_sensors");
        }

        // "cpu"/"gpu"/"cpuName"/"gpuName" keep the original one-shot format;
        // "gpu" is the last GPU found, as before. "gpus" lists every GPU.
        StringBuilder json = new StringBuilder();
        json.Append("{\"cpu\":").Append(Num(cpuSensor));
        json.Append(",\"gpu\":").Append(Num(lastGpuSensor));
        json.Append(",\"cpuName\":").Append(cpuName != null ? Str(cpuName) : "null");
        json.Append(",\"gpuName\":").Append(lastGpuName != null ? Str(lastGpuName) : "null");
        json.Append(",\"cpuSensor\":").Append(cpuSensor != null ? Str(cpuSensor.Name) : "null");
        json.Append(",\"gpus\":[").Append(string.Join(",", gpus.ToArray())).Append("]}");
        return json.ToString();
    }

    /// Find a temperature sensor matching any of the given name keywords (priority order),
    /// falling back to the first temperature sensor with a value.
    static ISensor FindTemp(IHardware hw, params string[] keywords)
    {
        ISensor fallback = null;
        foreach (string keyword in keywords)
        {
            foreach (ISensor sensor in hw.Sensors)
            {
                if (sensor.SensorType == SensorType.Temperature && sensor.Value.HasValue)
                {
                    if (sensor.Name.Contains(keyword))
                    {
                        return sensor;
                    }
                    if (fallback == null)
                    {
                        fallback = sensor;
                    }
                }
            }
        }
        return fallback;
    }

    /// Overall GPU load: "GPU Core" on NVIDIA/AMD, "D3D 3D" on Intel.
    static ISensor FindLoad(IHardware hw)
    {
        string[] keywords = { "GPU Core", "D3D 3D", "GPU" };
        foreach (string keyword in keywords)
        {
            foreach (ISensor sensor in hw.Sensors)
            {
                if (sensor.SensorType == SensorType.Load && sensor.Value.HasValue
                    && sensor.Name.Contains(keyword) && !sensor.Name.Contains("Memory"))
                {
                    return sensor;
                }
            }
        }
        return null;
    }

    static string Num(ISensor sensor)
    {
        if (sensor == null || !sensor.Value.HasValue) return "null";
        return sensor.Value.Value.ToString("F1", CultureInfo.InvariantCulture);
    }

    static string Str(string value)
    {
        return "\"" + value.Replace("\\", "\\\\").Replace("\"", "\\\"") + "\"";
    }
}
