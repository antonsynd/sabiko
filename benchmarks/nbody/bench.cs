// The n-body simulation of the Jovian planets; see bench.sabi. Mirrors the Sabiko
// program's shape: bodies are struct fields passed by `ref`, and the square root is the
// same Newton iteration (Sabiko has no f64::sqrt yet), so both compute the same thing.
using System;
using System.Globalization;

struct Body
{
    public double X, Y, Z, Vx, Vy, Vz, Mass;
}

struct Planets
{
    public Body Sun, Jupiter, Saturn, Uranus, Neptune;
}

static class Program
{
    const double Pi = 3.141592653589793;
    const double SolarMass = 4.0 * Pi * Pi;
    const double DaysPerYear = 365.24;
    const int Steps = 60_000;
    const double TimeStep = 0.01;

    static Body MakeBody(double x, double y, double z, double vx, double vy, double vz, double mass) =>
        new Body
        {
            X = x,
            Y = y,
            Z = z,
            Vx = vx * DaysPerYear,
            Vy = vy * DaysPerYear,
            Vz = vz * DaysPerYear,
            Mass = mass * SolarMass,
        };

    static double Sqrt(double x)
    {
        if (x <= 0.0) return 0.0;
        double guess = x > 1.0 ? x : 1.0;
        while (true)
        {
            double next = 0.5 * (guess + x / guess);
            if (next >= guess) return guess;
            guess = next;
        }
    }

    static void Interact(ref Body a, ref Body b, double dt)
    {
        double dx = a.X - b.X;
        double dy = a.Y - b.Y;
        double dz = a.Z - b.Z;
        double distanceSquared = dx * dx + dy * dy + dz * dz;
        double magnitude = dt / (distanceSquared * Sqrt(distanceSquared));
        double aMass = a.Mass * magnitude;
        double bMass = b.Mass * magnitude;
        a.Vx -= dx * bMass;
        a.Vy -= dy * bMass;
        a.Vz -= dz * bMass;
        b.Vx += dx * aMass;
        b.Vy += dy * aMass;
        b.Vz += dz * aMass;
    }

    static void Move(ref Body b, double dt)
    {
        b.X += dt * b.Vx;
        b.Y += dt * b.Vy;
        b.Z += dt * b.Vz;
    }

    static void Advance(ref Planets p, double dt)
    {
        Interact(ref p.Sun, ref p.Jupiter, dt);
        Interact(ref p.Sun, ref p.Saturn, dt);
        Interact(ref p.Sun, ref p.Uranus, dt);
        Interact(ref p.Sun, ref p.Neptune, dt);
        Interact(ref p.Jupiter, ref p.Saturn, dt);
        Interact(ref p.Jupiter, ref p.Uranus, dt);
        Interact(ref p.Jupiter, ref p.Neptune, dt);
        Interact(ref p.Saturn, ref p.Uranus, dt);
        Interact(ref p.Saturn, ref p.Neptune, dt);
        Interact(ref p.Uranus, ref p.Neptune, dt);
        Move(ref p.Sun, dt);
        Move(ref p.Jupiter, dt);
        Move(ref p.Saturn, dt);
        Move(ref p.Uranus, dt);
        Move(ref p.Neptune, dt);
    }

    static double Kinetic(in Body b) => 0.5 * b.Mass * (b.Vx * b.Vx + b.Vy * b.Vy + b.Vz * b.Vz);

    static double Potential(in Body a, in Body b)
    {
        double dx = a.X - b.X;
        double dy = a.Y - b.Y;
        double dz = a.Z - b.Z;
        return a.Mass * b.Mass / Sqrt(dx * dx + dy * dy + dz * dz);
    }

    static double Energy(in Planets p)
    {
        double e = 0.0;
        e += Kinetic(in p.Sun);
        e += Kinetic(in p.Jupiter);
        e += Kinetic(in p.Saturn);
        e += Kinetic(in p.Uranus);
        e += Kinetic(in p.Neptune);
        e -= Potential(in p.Sun, in p.Jupiter);
        e -= Potential(in p.Sun, in p.Saturn);
        e -= Potential(in p.Sun, in p.Uranus);
        e -= Potential(in p.Sun, in p.Neptune);
        e -= Potential(in p.Jupiter, in p.Saturn);
        e -= Potential(in p.Jupiter, in p.Uranus);
        e -= Potential(in p.Jupiter, in p.Neptune);
        e -= Potential(in p.Saturn, in p.Uranus);
        e -= Potential(in p.Saturn, in p.Neptune);
        e -= Potential(in p.Uranus, in p.Neptune);
        return e;
    }

    static void AddMomentum(ref Body total, in Body b)
    {
        total.Vx += b.Vx * b.Mass;
        total.Vy += b.Vy * b.Mass;
        total.Vz += b.Vz * b.Mass;
    }

    static void OffsetMomentum(ref Planets p)
    {
        Body total = MakeBody(0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
        AddMomentum(ref total, in p.Sun);
        AddMomentum(ref total, in p.Jupiter);
        AddMomentum(ref total, in p.Saturn);
        AddMomentum(ref total, in p.Uranus);
        AddMomentum(ref total, in p.Neptune);
        p.Sun.Vx = -total.Vx / SolarMass;
        p.Sun.Vy = -total.Vy / SolarMass;
        p.Sun.Vz = -total.Vz / SolarMass;
    }

    static void Main()
    {
        var p = new Planets
        {
            Sun = MakeBody(0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0),
            Jupiter = MakeBody(
                4.84143144246472090, -1.16032004402742839, -0.103622044471123109,
                0.00166007664274403694, 0.00769901118419740425, -0.0000690460016972063023,
                0.000954791938424326609),
            Saturn = MakeBody(
                8.34336671824457987, 4.12479856412430479, -0.403523417114321381,
                -0.00276742510726862411, 0.00499852801234917238, 0.0000230417297573763929,
                0.000285885980666130812),
            Uranus = MakeBody(
                12.8943695621391310, -15.1111514016986312, -0.223307578892655734,
                0.00296460137564761618, 0.00237847173959480950, -0.0000296589568540237556,
                0.0000436624404335156298),
            Neptune = MakeBody(
                15.3796971148509165, -25.9193146099879641, 0.179258772950371181,
                0.00268067772490389322, 0.00162824170038242295, -0.0000951592254519715870,
                0.0000515138902046611451),
        };
        OffsetMomentum(ref p);
        Console.WriteLine(Energy(in p).ToString(CultureInfo.InvariantCulture));
        for (int step = 0; step < Steps; step++)
        {
            Advance(ref p, TimeStep);
        }
        Console.WriteLine(Energy(in p).ToString(CultureInfo.InvariantCulture));
    }
}
