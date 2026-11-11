"""__all__ can export private declarations and renamed imports."""

from oxyde import Field, Model
from sibling import ExternalHelper as PublicHelper
from sibling import external_value as exported_value

__all__ = ["Entry", "PublicHelper"]
__all__ += ["exported_value", "_PrivateSupport"]
__all__.append("_private_constant")

_private_constant: int = 42


class _PrivateSupport:
    def label(self) -> str:
        return "private"


class Entry(Model):
    id: int | None = Field(default=None, db_pk=True)

    class Meta:
        is_table = True
