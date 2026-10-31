"""Give this embedded interpreter a working ``sys.executable``.

``pluginhost`` loads CPython as a shared library and calls ``Py_BytesMain``, which
never learns the program name: ``sys.executable`` comes up as ``''``. Anything
that re-executes the interpreter then fails with ``Permission denied: ''`` —
``ensurepip``'s own bootstrap, ``multiprocessing``'s spawn start method,
``subprocess.run([sys.executable, …])``.

The C shim resolves the launcher's real path before handing control to CPython
and exports it as ``ATOMCODE_PYTHON_EXE``; this module performs the assignment.
It runs automatically because ``site`` imports ``sitecustomize`` at startup.

Deliberately not done in C: ``Py_SetProgramName`` has no effect on the
``Py_BytesMain`` path, and reimplementing that entry point on
``Py_InitializeFromConfig`` is a much larger change than the problem warrants.
"""

import os
import sys

_launcher = os.environ.get("ATOMCODE_PYTHON_EXE")
if _launcher and os.path.isfile(_launcher) and sys.executable != _launcher:
    sys.executable = _launcher
