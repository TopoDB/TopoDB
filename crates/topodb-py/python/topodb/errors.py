class TopoDBError(Exception):
    """Base for every TopoDB error."""

class StorageError(TopoDBError): pass
class EncodingError(TopoDBError): pass
class RejectedError(TopoDBError): pass
class BusyError(TopoDBError):
    """Database file is held by another process. The operation may be retried."""

class CompactedError(TopoDBError):
    def __init__(self, msg, oldest=None):
        super().__init__(msg)
        self.oldest = oldest

class ClosedError(TopoDBError): pass

class UnsupportedFormatError(TopoDBError):
    def __init__(self, msg, found=None, supported=None):
        super().__init__(msg)
        self.found = found
        self.supported = supported
