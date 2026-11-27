"""Native rows and the adapter for MessagePack-only clients."""

from __future__ import annotations

from typing import TYPE_CHECKING, Any, Protocol

from oxyde._msgpack import msgpack
from oxyde.queries._result_types import convert_column, convert_rows

if TYPE_CHECKING:
    from oxyde.queries.base import SupportsExecute

type RelationResult = tuple[list[str], list[dict[str, Any]], list[int | None]]


class ResultSet(Protocol):
    @property
    def columns(self) -> list[str]: ...

    @property
    def relation_prefixes(self) -> list[str]: ...

    def __len__(self) -> int: ...
    def dicts(self, keys: list[str]) -> list[dict[str, Any]]: ...
    def tuples(self, indices: list[int | None]) -> list[tuple[Any, ...]]: ...
    def column(self, index: int) -> list[Any]: ...
    def relation(self, prefix: str) -> RelationResult | None: ...


def _unpack_result(
    data: Any,
) -> (
    tuple[list[str], list[list[Any]], dict[str, Any] | None, list[str | None] | None]
    | None
):
    """Read the columnar format, including its older two- and three-item forms."""
    if not isinstance(data, (list, tuple)) or len(data) not in (2, 3, 4):
        return None
    columns, rows = data[:2]
    if not isinstance(columns, list) or not all(isinstance(c, str) for c in columns):
        return None
    relations = data[2] if len(data) >= 3 and isinstance(data[2], dict) else None
    kinds = data[3] if len(data) == 4 else None
    return columns, rows, relations, kinds


class MsgpackResultSet:
    def __init__(self, data: Any) -> None:
        self.legacy_rows: list[dict[str, Any]] | None = None
        result = _unpack_result(data)
        if result is None:
            # Legacy dict payloads can have different keys on each row.
            self.legacy_rows = data if isinstance(data, list) else []
            self.columns = list(self.legacy_rows[0]) if self.legacy_rows else []
            self._rows: list[list[Any]] = []
            self._relations: dict[str, Any] | None = None
            self._kinds: list[str | None] | None = None
        else:
            self.columns, self._rows, self._relations, self._kinds = result

    @property
    def relation_prefixes(self) -> list[str]:
        return list(self._relations) if self._relations is not None else []

    def __len__(self) -> int:
        return len(self._rows if self.legacy_rows is None else self.legacy_rows)

    def dicts(self, keys: list[str]) -> list[dict[str, Any]]:
        if self.legacy_rows is not None:
            return self.legacy_rows
        self._convert_rows()
        return [dict(zip(keys, row)) for row in self._rows]

    def tuples(self, indices: list[int | None]) -> list[tuple[Any, ...]]:
        if self.legacy_rows is not None:
            fields = [self.columns[i] if i is not None else None for i in indices]
            return [
                tuple(row.get(field) if field is not None else None for field in fields)
                for row in self.legacy_rows
            ]
        self._convert_rows()
        return [
            tuple(row[i] if i is not None else None for i in indices)
            for row in self._rows
        ]

    def column(self, index: int) -> list[Any]:
        if self.legacy_rows is not None:
            key = self.columns[index]
            return [row.get(key) for row in self.legacy_rows]
        values = [row[index] for row in self._rows]
        if self._kinds is not None and (kind := self._kinds[index]) is not None:
            return convert_column(kind, values, standard_tz=True)
        return values

    def relation(self, prefix: str) -> RelationResult | None:
        if self._relations is None or (relation := self._relations.get(prefix)) is None:
            return None
        columns, data, refs = relation["columns"], relation["data"], relation["refs"]
        positions = {pk: index for index, pk in enumerate(data)}
        values = list(data.values())
        if (kinds := relation.get("kinds")) is not None and values:
            convert_rows(values, kinds, standard_tz=True)
        return (
            columns,
            [dict(zip(columns, row)) for row in values],
            [positions.get(pk) if pk is not None else None for pk in refs],
        )

    def _convert_rows(self) -> None:
        if self._kinds is not None and self._rows:
            convert_rows(self._rows, self._kinds, standard_tz=True)
            self._kinds = None


def _supports_execute_rows(client: SupportsExecute) -> bool:
    mro = type(client).__mro__
    i_exec = next((i for i, cls in enumerate(mro) if "execute" in vars(cls)), None)
    i_rows = next((i for i, cls in enumerate(mro) if "execute_rows" in vars(cls)), None)
    return (
        i_exec is not None
        and i_rows is not None
        and i_rows <= i_exec
        and getattr(client.execute, "__func__", None) is vars(mro[i_exec])["execute"]
    )


async def fetch_result(client: SupportsExecute, ir: dict[str, Any]) -> ResultSet:
    execute_rows = (
        getattr(client, "execute_rows", None)
        if _supports_execute_rows(client)
        else None
    )
    if callable(execute_rows):
        result: ResultSet = await execute_rows(ir)
        return result
    # Deduplicated relations can have non-string primary keys.
    return MsgpackResultSet(
        msgpack.unpackb(await client.execute(ir), raw=False, strict_map_key=False)
    )
