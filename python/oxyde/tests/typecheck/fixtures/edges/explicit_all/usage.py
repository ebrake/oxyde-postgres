from typing import assert_type

from module import Entry, PublicHelper, _PrivateSupport, _private_constant, exported_value

assert_type(exported_value, int)
assert_type(_private_constant, int)
assert_type(PublicHelper().label(), str)
assert_type(_PrivateSupport().label(), str)


async def fetch_entry() -> Entry:
    return await Entry.objects.get(id=1)
