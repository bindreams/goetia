// The container's main process: answers every console event as handled, so that a container shutdown (which
// sends the main process CTRL_SHUTDOWN_EVENT) leaves it running, and the services are what the shutdown waits for.
// idle.exe <dir> writes <dir>\ready once its handler is in place, then waits for ever.
using System;
using System.IO;
using System.Runtime.InteropServices;
using System.Threading;

static class Idle
{
    delegate bool Handler(uint type);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool SetConsoleCtrlHandler(Handler handler, bool add);

    // Held for the life of the process, so the delegate is never collected.
    static readonly Handler Keep = type => true;

    static int Main(string[] args)
    {
        if (!SetConsoleCtrlHandler(Keep, true))
        {
            Console.Error.WriteLine("SetConsoleCtrlHandler failed: " + Marshal.GetLastWin32Error());
            return 1;
        }
        File.WriteAllText(Path.Combine(args[0], "ready"), "ready");
        new ManualResetEvent(false).WaitOne();
        return 0;
    }
}
