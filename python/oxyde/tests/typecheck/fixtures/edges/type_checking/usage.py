from typing import assert_type

from module import Company
from sibling import Auditor, Editor, Reviewer, User


def nullable_relations(company: Company) -> None:
    assert_type(company.contact_user, User | None)
    assert_type(company.reviewer, Reviewer | None)
    assert_type(company.editor, Editor | None)
    assert_type(company.auditor, Auditor | None)


async def filter_company() -> Company:
    return await Company.objects.filter(id=1, contact_user__isnull=True).get()
