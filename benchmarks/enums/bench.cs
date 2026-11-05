// Enums and `match` in a hot loop, as tagged structs and `switch`; see bench.sabi.
using System;

enum OpKind { Push, Add, Mul, Dup, Swap, Mod, JumpIfSmall, Halt }

struct Op
{
    public OpKind Kind;
    public long A, B;

    public Op(OpKind kind, long a = 0, long b = 0)
    {
        Kind = kind;
        A = a;
        B = b;
    }
}

enum LightKind { Red, Green, Yellow, Blinking }

struct Light
{
    public LightKind Kind;
    public long N;

    public Light(LightKind kind, long n = 0)
    {
        Kind = kind;
        N = n;
    }
}

struct Machine
{
    public long A, B, Depth;

    public void Push(long v)
    {
        B = A;
        A = v;
        Depth += 1;
    }

    public long Pop()
    {
        long v = A;
        A = B;
        Depth -= 1;
        return v;
    }
}

static class Program
{
    const long Rounds = 2_000_000;
    const long Modulus = 1_000_003;

    static Op Fetch(long pc, long seed) => pc switch
    {
        0 => new Op(OpKind.Push, seed),
        1 => new Op(OpKind.Push, 31),
        2 => new Op(OpKind.Mul),
        3 => new Op(OpKind.Push, 7),
        4 => new Op(OpKind.Add),
        5 => new Op(OpKind.Mod, Modulus),
        6 => new Op(OpKind.Dup),
        7 => new Op(OpKind.Swap),
        8 => new Op(OpKind.Add),
        9 => new Op(OpKind.JumpIfSmall, 1000, 1),
        _ => new Op(OpKind.Halt),
    };

    static long Run(long seed)
    {
        var m = new Machine();
        long pc = 0;
        long steps = 0;
        while (true)
        {
            steps += 1;
            Op op = Fetch(pc, seed);
            switch (op.Kind)
            {
                case OpKind.Push:
                    m.Push(op.A);
                    break;
                case OpKind.Add:
                {
                    long x = m.Pop();
                    long y = m.Pop();
                    m.Push((x + y) % Modulus);
                    break;
                }
                case OpKind.Mul:
                {
                    long x = m.Pop();
                    long y = m.Pop();
                    m.Push(x * y % Modulus);
                    break;
                }
                case OpKind.Dup:
                    m.Push(m.A);
                    break;
                case OpKind.Swap:
                {
                    long x = m.A;
                    m.A = m.B;
                    m.B = x;
                    break;
                }
                case OpKind.Mod:
                {
                    long x = m.Pop();
                    m.Push(x % op.A);
                    break;
                }
                case OpKind.JumpIfSmall:
                    if (m.A < op.A)
                    {
                        pc = op.B;
                        continue;
                    }
                    break;
                case OpKind.Halt:
                    return m.A + steps + m.Depth;
            }
            pc += 1;
        }
    }

    static Light Next(Light state, long input)
    {
        long k = input % 4;
        switch (state.Kind)
        {
            case LightKind.Red:
                return k == 0 || k == 1 ? new Light(LightKind.Green) : new Light(LightKind.Red);
            case LightKind.Green:
                return k == 3 ? new Light(LightKind.Blinking) : new Light(LightKind.Yellow, input % 5);
            case LightKind.Yellow:
                return state.N > 0 ? new Light(LightKind.Yellow, state.N - 1) : new Light(LightKind.Red);
            default:
                return k == 2 ? new Light(LightKind.Red) : new Light(LightKind.Blinking);
        }
    }

    static long Score(Light l) => l.Kind switch
    {
        LightKind.Red => 1,
        LightKind.Green => 2,
        LightKind.Yellow => 3 + l.N,
        _ => 7,
    };

    static void Main()
    {
        long total = 0;
        var state = new Light(LightKind.Red);
        for (long i = 0; i < Rounds; i++)
        {
            if (i % 64 == 0)
            {
                total = (total + Run(i % 977)) % Modulus;
            }
            state = Next(state, i * 7 + total);
            total = (total + Score(state)) % Modulus;
        }
        Console.WriteLine(total);
    }
}
