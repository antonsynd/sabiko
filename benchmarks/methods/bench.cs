// Methods in a hot loop, as instance methods on structs; see bench.sabi.
using System;

struct Rng
{
    public ulong State;

    public Rng(ulong seed) => State = seed;

    public long Next()
    {
        State = State * 48271 % 2147483647;
        return (long)(State % 1000) - 500;
    }
}

struct Vec2
{
    const long Modulus = 1_000_003;
    public long X, Y;

    public Vec2(long x, long y)
    {
        X = x;
        Y = y;
    }

    public readonly Vec2 Plus(Vec2 other) => new Vec2((X + other.X) % Modulus, (Y + other.Y) % Modulus);

    public readonly long Dot(in Vec2 other) => (X * other.X + Y * other.Y) % Modulus;

    public void Damp()
    {
        X = X * 15 / 16;
        Y = Y * 15 / 16;
    }
}

struct Particle
{
    public Vec2 Pos, Vel;

    public static Particle At(long x, long y) => new Particle { Pos = new Vec2(x, y), Vel = new Vec2(0, 0) };

    public void Step(ref Rng rng)
    {
        Vel = Vel.Plus(new Vec2(rng.Next(), rng.Next()));
        Vel.Damp();
        Pos = Pos.Plus(Vel);
    }

    public readonly long Energy() => Vel.Dot(in Vel);
}

struct World
{
    const long Modulus = 1_000_003;
    public Particle A, B;
    public long Total;

    public void Tick(ref Rng rng)
    {
        A.Step(ref rng);
        B.Step(ref rng);
        Add(A.Energy() + B.Energy());
    }

    public void Add(long energy) => Total = (Total + energy) % Modulus;
}

static class Program
{
    const ulong Rounds = 6_000_000;

    static void Main()
    {
        var rng = new Rng(42);
        var world = new World { A = Particle.At(1, 2), B = Particle.At(-3, 4), Total = 0 };
        for (ulong i = 0; i < Rounds; i++)
        {
            world.Tick(ref rng);
        }
        Console.WriteLine(world.Total);
        Console.WriteLine(world.A.Pos.X);
        Console.WriteLine(world.A.Pos.Y);
        Console.WriteLine(world.B.Pos.X);
        Console.WriteLine(world.B.Pos.Y);
    }
}
