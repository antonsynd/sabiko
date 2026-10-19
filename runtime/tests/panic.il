// Runtime panic regression probe: the shape the generated entry-point wrapper has. The user's
// Main panics; the wrapper's catch hands the exception to Panic::Unhandled and returns its
// value, so the process exits with 101 and stderr carries the panic message.
// Run scripts/check-runtime.sh.
.assembly extern System.Runtime { .ver 10:0:0:0 .publickeytoken = (B0 3F 5F 7F 11 D5 0A 3A) }
.assembly extern Sabiko.Runtime { .ver 0:1:0:0 }
.assembly Panic { .ver 0:1:0:0 }
.module Panic.exe

.class public abstract auto ansi sealed beforefieldinit Program extends [System.Runtime]System.Object
{
  .method public hidebysig static void UserMain() cil managed
  {
    .maxstack 1
    ldstr "before"
    call void [Sabiko.Runtime]Sabiko.Runtime.Console::PrintLine(string)
    ldstr "boom"
    call void [Sabiko.Runtime]Sabiko.Runtime.Panic::Fail(string)
    ldstr "not reached"
    call void [Sabiko.Runtime]Sabiko.Runtime.Console::PrintLine(string)
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
