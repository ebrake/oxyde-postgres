from oxyde import Field, Model


class User(Model):
    id: int | None = Field(default=None, db_pk=True)

    class Meta:
        is_table = True


class Reviewer(Model):
    id: int | None = Field(default=None, db_pk=True)

    class Meta:
        is_table = True


class Editor(Model):
    id: int | None = Field(default=None, db_pk=True)

    class Meta:
        is_table = True


class Auditor(Model):
    id: int | None = Field(default=None, db_pk=True)

    class Meta:
        is_table = True
