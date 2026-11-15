"""Measure regular database files after the node has stopped; exclude directory metadata."""
import os
import stat

if os.name == 'nt':
    import ctypes
    from ctypes import wintypes
    import msvcrt

    # https://learn.microsoft.com/windows/win32/api/winbase/ns-winbase-file_standard_info
    class FileStandardInfo(ctypes.Structure):
        _fields_ = [('allocation', ctypes.c_longlong), ('end', ctypes.c_longlong),
                    ('links', wintypes.DWORD), ('delete_pending', ctypes.c_ubyte),
                    ('directory', ctypes.c_ubyte)]

    _info = ctypes.WinDLL('kernel32', use_last_error=True).GetFileInformationByHandleEx
    _info.argtypes = [wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD]
    _info.restype = wintypes.BOOL


def closed_directory_footprint(directory):
    logical = allocated = files = 0
    for path in directory.rglob('*'):
        metadata = path.lstat()
        if not stat.S_ISREG(metadata.st_mode):
            continue
        logical += metadata.st_size
        files += 1
        if os.name == 'nt':
            data = FileStandardInfo()
            with path.open('rb') as source:
                handle = wintypes.HANDLE(msvcrt.get_osfhandle(source.fileno()))
                if not _info(handle, 1, ctypes.byref(data), ctypes.sizeof(data)):
                    raise ctypes.WinError(ctypes.get_last_error())
            allocated += data.allocation
        else:
            allocated += metadata.st_blocks * 512
    return {'files': files, 'logicalBytes': logical, 'allocatedBytes': allocated,
            'allocationSource': 'FILE_STANDARD_INFO' if os.name == 'nt' else 'st_blocks*512'}
