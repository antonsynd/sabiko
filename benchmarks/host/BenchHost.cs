// In-process benchmark host: loads a benchmark executable (Sabiko or C#) into this
// process and calls its entry point repeatedly, so JIT and runtime start-up are paid
// once and every measured iteration runs steady-state code.
//
// Usage: dotnet BenchHost.dll <bench.dll> <warmup> <iterations> <result.json>
//
// The benchmark's standard output goes to a writer installed before the assembly is
// loaded: the first warmup iteration's output is captured (the runner compares it with
// the expected output), every later iteration writes to TextWriter.Null. Sabiko.Runtime
// caches Console.Out on first use, which is why the writer is a forwarding one installed
// up front rather than swapped with Console.SetOut later.
//
// Per measured iteration the host records the Stopwatch time of the entry-point call and
// the managed bytes allocated during it (GC.GetTotalAllocatedBytes(precise: true), read
// outside the timed region); over the measured iterations it records how many collections
// of each generation ran. A non-zero exit code from the entry point (Sabiko's wrapper
// returns 101 after reporting a panic) fails the run.
using System;
using System.Diagnostics;
using System.Globalization;
using System.IO;
using System.Reflection;
using System.Runtime.InteropServices;
using System.Text;

static class BenchHost
{
    static int Main(string[] args)
    {
        if (args.Length != 4)
        {
            Console.Error.WriteLine("usage: BenchHost <bench.dll> <warmup> <iterations> <result.json>");
            return 2;
        }
        string path = Path.GetFullPath(args[0]);
        int warmup = int.Parse(args[1], CultureInfo.InvariantCulture);
        int iterations = int.Parse(args[2], CultureInfo.InvariantCulture);
        string resultPath = args[3];

        var capture = new StringWriter(CultureInfo.InvariantCulture) { NewLine = "\n" };
        var output = new ForwardingWriter(capture);
        Console.SetOut(output);

        Func<int> run = EntryPoint(Assembly.LoadFrom(path));

        // The first warmup iteration's output is the program's output; there is always one.
        int code = run();
        if (code != 0)
        {
            Console.Error.WriteLine($"BenchHost: {path} exited with code {code}");
            return 1;
        }
        string firstOutput = capture.ToString();
        output.Target = TextWriter.Null;
        for (int i = 1; i < warmup; i++)
        {
            code = run();
            if (code != 0)
            {
                Console.Error.WriteLine($"BenchHost: {path} exited with code {code} in warmup {i}");
                return 1;
            }
        }

        var elapsedNs = new long[iterations];
        var allocated = new long[iterations];
        double nsPerTick = 1e9 / Stopwatch.Frequency;
        int gen0 = GC.CollectionCount(0), gen1 = GC.CollectionCount(1), gen2 = GC.CollectionCount(2);
        for (int i = 0; i < iterations; i++)
        {
            long before = GC.GetTotalAllocatedBytes(precise: true);
            long start = Stopwatch.GetTimestamp();
            code = run();
            long end = Stopwatch.GetTimestamp();
            long after = GC.GetTotalAllocatedBytes(precise: true);
            if (code != 0)
            {
                Console.Error.WriteLine($"BenchHost: {path} exited with code {code} in iteration {i}");
                return 1;
            }
            elapsedNs[i] = (long)((end - start) * nsPerTick);
            allocated[i] = after - before;
        }
        gen0 = GC.CollectionCount(0) - gen0;
        gen1 = GC.CollectionCount(1) - gen1;
        gen2 = GC.CollectionCount(2) - gen2;

        var json = new StringBuilder();
        json.Append("{\n");
        json.Append("  \"runtime\": ").Append(Quote(RuntimeInformation.FrameworkDescription)).Append(",\n");
        json.Append("  \"warmup\": ").Append(warmup).Append(",\n");
        json.Append("  \"iterations\": ").Append(iterations).Append(",\n");
        json.Append("  \"output\": ").Append(Quote(firstOutput)).Append(",\n");
        json.Append("  \"elapsed_ns\": ").Append(Array(elapsedNs)).Append(",\n");
        json.Append("  \"allocated_bytes\": ").Append(Array(allocated)).Append(",\n");
        json.Append("  \"gc_collections\": {\"gen0\": ").Append(gen0)
            .Append(", \"gen1\": ").Append(gen1)
            .Append(", \"gen2\": ").Append(gen2).Append("},\n");
        json.Append("  \"peak_working_set_bytes\": ")
            .Append(Process.GetCurrentProcess().PeakWorkingSet64).Append('\n');
        json.Append("}\n");
        File.WriteAllText(resultPath, json.ToString());
        return 0;
    }

    /// <summary>Binds the assembly's entry point to a delegate (no per-call reflection or boxing).</summary>
    static Func<int> EntryPoint(Assembly assembly)
    {
        MethodInfo entry = assembly.EntryPoint
            ?? throw new InvalidOperationException($"{assembly.Location} has no entry point");
        bool takesArgs = entry.GetParameters().Length == 1;
        if (entry.ReturnType == typeof(int))
        {
            if (takesArgs)
            {
                var main = entry.CreateDelegate<Func<string[], int>>();
                string[] none = [];
                return () => main(none);
            }
            return entry.CreateDelegate<Func<int>>();
        }
        if (takesArgs)
        {
            var main = entry.CreateDelegate<Action<string[]>>();
            string[] none = [];
            return () => { main(none); return 0; };
        }
        var action = entry.CreateDelegate<Action>();
        return () => { action(); return 0; };
    }

    static string Array(long[] values)
    {
        var sb = new StringBuilder("[");
        for (int i = 0; i < values.Length; i++)
        {
            if (i > 0) sb.Append(", ");
            sb.Append(values[i].ToString(CultureInfo.InvariantCulture));
        }
        return sb.Append(']').ToString();
    }

    static string Quote(string s)
    {
        var sb = new StringBuilder("\"");
        foreach (char c in s)
        {
            switch (c)
            {
                case '"': sb.Append("\\\""); break;
                case '\\': sb.Append("\\\\"); break;
                case '\n': sb.Append("\\n"); break;
                case '\r': sb.Append("\\r"); break;
                case '\t': sb.Append("\\t"); break;
                default:
                    if (c < ' ') sb.Append("\\u").Append(((int)c).ToString("x4", CultureInfo.InvariantCulture));
                    else sb.Append(c);
                    break;
            }
        }
        return sb.Append('"').ToString();
    }
}

/// <summary>A writer whose destination can change after Console.Out has been cached.</summary>
sealed class ForwardingWriter : TextWriter
{
    public TextWriter Target;

    public ForwardingWriter(TextWriter target) : base(CultureInfo.InvariantCulture)
    {
        Target = target;
        NewLine = "\n";
    }

    public override Encoding Encoding => Encoding.UTF8;
    public override void Write(char value) => Target.Write(value);
    public override void Write(char[] buffer, int index, int count) => Target.Write(buffer, index, count);
    public override void Write(ReadOnlySpan<char> buffer) => Target.Write(buffer);
    public override void Write(string? value) => Target.Write(value);
    public override void WriteLine(string? value) => Target.WriteLine(value);
    public override void WriteLine(ReadOnlySpan<char> buffer) => Target.WriteLine(buffer);
    public override void Flush() => Target.Flush();
}
