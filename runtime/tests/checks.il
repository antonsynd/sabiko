// Runtime check-failure probe: the shape generated code gives a failed run-time check
// (decision 43, issue #29). The location is loaded, the throw helper in
// Sabiko.Runtime.Checks raises the PanicException with Rust's message, and the unreachable
// `ldnull; throw` after the call ends the block. Add(1, 2) prints 3; Add(i32::MAX, 1)
// panics, so the process exits with 101 and stderr carries Rust's report:
// "thread 'main' panicked at src/probe.sabi:3:9:" and "attempt to add with overflow".
// Run scripts/check-runtime.sh.
.assembly extern System.Runtime { .ver 10:0:0:0 .publickeytoken = (B0 3F 5F 7F 11 D5 0A 3A) }
.assembly extern Sabiko.Runtime { .ver 0:1:0:0 }
.assembly Checks { .ver 0:1:0:0 }
.module Checks.exe

.class public abstract auto ansi sealed beforefieldinit Program extends [System.Runtime]System.Object
{
  .method public hidebysig static int32 Add(int32 a, int32 b) cil managed
  {
    .maxstack 3
    ldarg.0
    conv.i8
    ldarg.1
    conv.i8
    add
    dup
    dup
    conv.i4
    conv.i8
    beq OK
    ldstr "src/probe.sabi:3:9"
    call void [Sabiko.Runtime]Sabiko.Runtime.Checks::AddOverflow(string)
    ldnull
    throw
  OK:
    conv.i4
    ret
  }

  .method public hidebysig static void UserMain() cil managed
  {
    .maxstack 2
    ldc.i4.1
    ldc.i4.2
    call int32 Program::Add(int32, int32)
    call void [Sabiko.Runtime]Sabiko.Runtime.Console::PrintLine(int32)
    ldc.i4 2147483647
    ldc.i4.1
    call int32 Program::Add(int32, int32)
    call void [Sabiko.Runtime]Sabiko.Runtime.Console::PrintLine(int32)
    ret
  }

  .method public hidebysig static int32 Main() cil managed
  {
    .entrypoint
    .maxstack 1
    .locals init ([0] int32 code)
    ldc.i4.0
    stloc.0
    .try
    {
      call void Program::UserMain()
      leave.s done
    }
    catch [System.Runtime]System.Exception
    {
      call int32 [Sabiko.Runtime]Sabiko.Runtime.Panic::Unhandled(class [System.Runtime]System.Exception)
      stloc.0
      leave.s done
    }
  done:
    ldloc.0
    ret
  }
}
