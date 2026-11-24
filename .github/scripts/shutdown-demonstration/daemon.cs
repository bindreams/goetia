// The two daemons of the shutdown demonstration. Each writes its ready file once its console handler is in place.
//
//   daemon.exe marker <marker-file> <ready-file>   writes <marker-file> when it is asked to exit with Ctrl+C or
//                                                  Ctrl+Break, then exits: a daemon that shuts down by itself.
//   daemon.exe ignore <ready-file>                 answers every console event as handled and runs until it is ended.
using System;
using System.IO;
using System.Runtime.InteropServices;
using System.Threading;

static class Daemon
{
    const uint CTRL_C_EVENT = 0;
    const uint CTRL_BREAK_EVENT = 1;

    delegate bool Handler(uint type);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool SetConsoleCtrlHandler(Handler handler, bool add);

    // Held for the life of the process, so the delegate is never collected.
    static Handler handler;

    static int Main(string[] args)
    {
        string ready;
        if (args[0] == "marker")
        {
            string marker = args[1];
            ready = args[2];
            handler = type =>
            {
                if (type != CTRL_C_EVENT && type != CTRL_BREAK_EVENT)
                {
                    return false;
                }
                File.WriteAllText(marker, "asked with console event " + type);
                Environment.Exit(0);
                return true;
            };
        }
        else
        {
            ready = args[1];
            handler = type => true;
        }
        if (!SetConsoleCtrlHandler(handler, true))
        {
            Console.Error.WriteLine("SetConsoleCtrlHandler failed: " + Marshal.GetLastWin32Error());
            return 1;
        }
        Console.WriteLine(args[0] + ": console handler in place");
        File.WriteAllText(ready, "ready");
        new ManualResetEvent(false).WaitOne();
        return 0;
    }
}
