using System;
using LibreHardwareMonitor.Hardware;

class ThermalDebug
{
    static void Main()
    {
        var computer = new Computer
        {
            IsCpuEnabled = true,
            IsGpuEnabled = true
        };
        computer.Open();

        foreach (IHardware hw in computer.Hardware)
        {
            hw.Update();
            Console.WriteLine("Hardware: {0} (Type: {1})", hw.Name, hw.HardwareType);
            
            foreach (ISensor sensor in hw.Sensors)
            {
                if (sensor.SensorType == SensorType.Temperature)
                {
                    Console.WriteLine("  Sensor: {0} = {1}°C", sensor.Name, sensor.Value);
                }
            }
            
            foreach (IHardware sub in hw.SubHardware)
            {
                sub.Update();
                Console.WriteLine("  SubHardware: {0} (Type: {1})", sub.Name, sub.HardwareType);
                foreach (ISensor sensor in sub.Sensors)
                {
                    if (sensor.SensorType == SensorType.Temperature)
                    {
                        Console.WriteLine("    Sensor: {0} = {1}°C", sensor.Name, sensor.Value);
                    }
                }
            }
        }

        computer.Close();
    }
}
