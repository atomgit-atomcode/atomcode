/*
 * `_posixshmem` for Android, where bionic has no POSIX shared memory.
 *
 * CPython only builds this module when configure finds `shm_open`/`shm_unlink`,
 * and bionic provides neither (measured: `call to undeclared function
 * 'shm_open'`). The consequence is not just a missing module: `multiprocessing`'s
 * resource tracker imports it unconditionally, so the whole `spawn` start method
 * and `shared_memory` fail to load.
 *
 * The stdlib only needs "open by name → get an fd → truncate/fstat/mmap it →
 * later unlink by name". A plain file under `$TMPDIR` gives exactly that. Names
 * are reduced to a single flat leaf so a crafted name cannot escape the
 * directory, which is the one security property the real `shm_open` has.
 *
 * Build:
 *   cc -shared -fPIC -I<python>/include/python3.12 -o _posixshmem.cpython-312.so \
 *      posixshmem.c -L<python>/lib -lpython3.12
 */
#define PY_SSIZE_T_CLEAN
#include <Python.h>

#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static int build_path(const char *name, char *out, size_t size) {
    const char *tmp = getenv("TMPDIR");
    if (tmp == NULL || *tmp == '\0') {
        tmp = "/tmp";
    }
    const char *leaf = name;
    while (*leaf == '/') {
        leaf++;
    }
    if (*leaf == '\0' || strchr(leaf, '/') != NULL) {
        errno = EINVAL;
        return -1;
    }
    if (snprintf(out, size, "%s/pshm-%s", tmp, leaf) <= 0) {
        errno = ENAMETOOLONG;
        return -1;
    }
    return 0;
}

static PyObject *posixshm_shm_open(PyObject *module, PyObject *args) {
    (void)module;
    const char *name = NULL;
    int flags = 0;
    int mode = 0600;
    if (!PyArg_ParseTuple(args, "si|i:shm_open", &name, &flags, &mode)) {
        return NULL;
    }
    char path[PATH_MAX + 1];
    if (build_path(name, path, sizeof(path)) != 0) {
        return PyErr_SetFromErrno(PyExc_OSError);
    }
    int fd = open(path, flags, (mode_t)mode);
    if (fd < 0) {
        return PyErr_SetFromErrnoWithFilename(PyExc_OSError, name);
    }
    return PyLong_FromLong(fd);
}

static PyObject *posixshm_shm_unlink(PyObject *module, PyObject *args) {
    (void)module;
    const char *name = NULL;
    if (!PyArg_ParseTuple(args, "s:shm_unlink", &name)) {
        return NULL;
    }
    char path[PATH_MAX + 1];
    if (build_path(name, path, sizeof(path)) != 0) {
        return PyErr_SetFromErrno(PyExc_OSError);
    }
    /* Already gone is what the caller wants; report only real failures. */
    if (unlink(path) != 0 && errno != ENOENT) {
        return PyErr_SetFromErrnoWithFilename(PyExc_OSError, name);
    }
    Py_RETURN_NONE;
}

static PyMethodDef posixshm_methods[] = {
    {"shm_open", posixshm_shm_open, METH_VARARGS,
     "open a shared memory object; returns a file descriptor"},
    {"shm_unlink", posixshm_shm_unlink, METH_VARARGS,
     "remove a shared memory object by name"},
    {NULL, NULL, 0, NULL},
};

static struct PyModuleDef posixshm_module = {
    PyModuleDef_HEAD_INIT,
    "_posixshmem",
    "POSIX shared memory, backed by files under $TMPDIR (bionic has none).",
    -1,
    posixshm_methods,
};

PyMODINIT_FUNC PyInit__posixshmem(void) {
    return PyModule_Create(&posixshm_module);
}
