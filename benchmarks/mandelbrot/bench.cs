// Escape-time Mandelbrot over a grid; see bench.sabi.
using System;

static class Program
{
    const int Width = 640;
    const int Height = 480;
    const int MaxIterations = 300;

    static int Escape(double cr, double ci)
    {
        double zr = 0.0, zi = 0.0;
        for (int i = 0; i < MaxIterations; i++)
        {
            double zr2 = zr * zr;
            double zi2 = zi * zi;
            if (zr2 + zi2 > 4.0) return i;
            zi = 2.0 * zr * zi + ci;
            zr = zr2 - zi2 + cr;
        }
        return MaxIterations;
    }

    static void Main()
    {
        long total = 0;
        int inside = 0;
        for (int y = 0; y < Height; y++)
        {
            double ci = -1.2 + 2.4 * y / Height;
            for (int x = 0; x < Width; x++)
            {
                double cr = -2.1 + 3.0 * x / Width;
                int n = Escape(cr, ci);
                total += n;
                if (n == MaxIterations) inside++;
            }
        }
        Console.WriteLine(total);
        Console.WriteLine(inside);
    }
}
