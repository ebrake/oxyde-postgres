"""Nullable relations under direct, qualified and aliased typing guards."""

from __future__ import annotations

import typing
import typing as t
from typing import TYPE_CHECKING
from typing import TYPE_CHECKING as TC

from oxyde import Field, Model

if TYPE_CHECKING:
    from sibling import User

if typing.TYPE_CHECKING:
    from sibling import Reviewer

if t.TYPE_CHECKING:
    from sibling import Editor

if TC:
    from sibling import Auditor


class Company(Model):
    id: int | None = Field(default=None, db_pk=True)
    contact_user: User | None = Field(default=None, db_fk="id")
    reviewer: Reviewer | None = Field(default=None, db_fk="id")
    editor: Editor | None = Field(default=None, db_fk="id")
    auditor: Auditor | None = Field(default=None, db_fk="id")

    class Meta:
        is_table = True


# Register the relations without introducing their unqualified runtime names.
import sibling
