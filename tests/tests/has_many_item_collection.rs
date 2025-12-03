use std::collections::HashMap;
use std_util::{assert_empty, assert_err, assert_none, num::NumUtil};
use tests::{models, tests, DbTest};
use toasty::stmt::Id;

#[allow(unused)]
async fn validates_pk_missing_parent(test: &mut DbTest) {
    #[derive(Debug, toasty::Model)]
    struct User {
        #[key]
        #[auto]
        id: Id<Self>,

        #[has_many]
        todos: toasty::HasMany<Todo>,
    }

    #[derive(Debug, toasty::Model)]
    #[item_collection(User)]
    struct Todo {
        #[key]
        #[auto]
        id: Id<Self>,

        #[index]
        user_id: Id<User>,

        #[belongs_to(key = user_id, references = id)]
        user: toasty::BelongsTo<User>,

        title: String,
    }

    assert_err!(test.try_setup_db(models!(User, Todo)).await);
}

#[allow(unused)]
async fn validates_pk_missing_primitive(test: &mut DbTest) {
    #[derive(Debug, toasty::Model)]
    struct User {
        #[key]
        #[auto]
        id: Id<Self>,

        #[has_many]
        todos: toasty::HasMany<Todo>,
    }

    #[derive(Debug, toasty::Model)]
    #[item_collection(User)]
    struct Todo {
        #[auto]
        id: Id<Self>,

        #[key]
        #[index]
        user_id: Id<User>,

        #[belongs_to(key = user_id, references = id)]
        user: toasty::BelongsTo<User>,

        title: String,
    }

    assert_err!(test.try_setup_db(models!(User, Todo)).await);
}

async fn crud_user_todos(test: &mut DbTest) {
    #[derive(Debug, toasty::Model)]
    struct User {
        #[key]
        #[auto]
        id: Id<Self>,

        #[has_many]
        todos: toasty::HasMany<Todo>,
    }

    #[derive(Debug, toasty::Model)]
    #[item_collection(User)]
    #[key(partition = user_id, local = id)]
    struct Todo {
        #[auto]
        id: Id<Self>,

        #[index]
        user_id: Id<User>,

        #[belongs_to(key = user_id, references = id)]
        user: toasty::BelongsTo<User>,

        title: String,
    }

    let item_collection = <Todo as toasty::Model>::schema().item_collection;
    assert_eq!(item_collection, Some(<User as toasty::Model>::id()));

    let db = test.setup_db(models!(User, Todo)).await;

    let schema = db.schema();
    println!("schema={schema:#?}");

    // Create a user
    let user = User::create().exec(&db).await.unwrap();

    // No TODOs
    assert_empty!(user
        .todos()
        .all(&db)
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await
        .unwrap());

    // Create a Todo associated with the user
    let todo = user
        .todos()
        .create()
        .title("hello world")
        .exec(&db)
        .await
        .unwrap();

    // Find the todo by ID
    let list = Todo::filter_by_user_id_and_id(&user.id, &todo.id)
        .all(&db)
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await
        .unwrap();

    assert_eq!(1, list.len());
    assert_eq!(todo.id, list[0].id);

    // Find the TODO by user ID
    let list = Todo::filter_by_user_id(&user.id)
        .all(&db)
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await
        .unwrap();

    assert_eq!(1, list.len());
    assert_eq!(todo.id, list[0].id);

    // Find the User using the Todo
    let user_reload = User::get_by_id(&db, &todo.user_id).await.unwrap();
    assert_eq!(user.id, user_reload.id);

    let mut created = HashMap::new();
    let mut ids = vec![(todo.user_id.clone(), todo.id.clone())];
    created.insert(todo.id.clone(), todo);

    // Create a few more TODOs
    for i in 0..5 {
        let title = format!("hello world {i}");

        let todo = if i.is_even() {
            // Create via user
            user.todos().create().title(title).exec(&db).await.unwrap()
        } else {
            // Create via todo builder
            Todo::create()
                .user(&user)
                .title(title)
                .exec(&db)
                .await
                .unwrap()
        };

        ids.push((todo.user_id.clone(), todo.id.clone()));
        assert_none!(created.insert(todo.id.clone(), todo));
    }

    // Load all TODOs
    let list = user
        .todos()
        .all(&db)
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await
        .unwrap();

    assert_eq!(6, list.len());

    let loaded: HashMap<_, _> = list
        .into_iter()
        .map(|todo| (todo.id.clone(), todo))
        .collect();
    assert_eq!(6, loaded.len());

    for (id, expect) in &created {
        assert_eq!(expect.title, loaded[id].title);
    }

    // Find all TODOs by user (using the belongs_to queries)
    let list = Todo::filter_by_user_id(&user.id)
        .collect::<Vec<_>>(&db)
        .await
        .unwrap();
    assert_eq!(6, list.len());

    let by_id: HashMap<_, _> = list
        .into_iter()
        .map(|todo| (todo.id.clone(), todo))
        .collect();

    assert_eq!(6, by_id.len());

    for (id, expect) in by_id {
        assert_eq!(expect.title, loaded[&id].title);
    }

    // Create a second user
    let user2 = User::create().exec(&db).await.unwrap();

    // No TODOs associated with `user2`
    assert_empty!(user2
        .todos()
        .all(&db)
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await
        .unwrap());

    // Create a TODO for user2
    let u2_todo = user2
        .todos()
        .create()
        .title("user 2 todo")
        .exec(&db)
        .await
        .unwrap();

    {
        let mut u1_todos = user.todos().all(&db).await.unwrap();

        while let Some(todo) = u1_todos.next().await {
            let todo = todo.unwrap();
            assert_ne!(u2_todo.id, todo.id);
        }
    }

    // Delete a TODO by value
    let todo = Todo::get_by_user_id_and_id(&db, &ids[0].0, &ids[0].1)
        .await
        .unwrap();
    todo.delete(&db).await.unwrap();

    // Can no longer get the todo via id
    assert_err!(Todo::get_by_user_id_and_id(&db, &ids[0].0, &ids[0].1).await);

    // Can no longer get the todo scoped
    assert_err!(user.todos().get_by_id(&db, &ids[0].1).await);

    // Delete a TODO by scope
    user.todos()
        .filter_by_id(&ids[1].1)
        .delete(&db)
        .await
        .unwrap();

    // Can no longer get the todo via id
    assert_err!(Todo::get_by_user_id_and_id(&db, &ids[1].0, &ids[1].1).await);

    // Can no longer get the todo scoped
    assert_err!(user.todos().get_by_id(&db, &ids[1].1).await);

    // Successfuly a todo by scope
    user.todos()
        .filter_by_id(&ids[2].1)
        .update()
        .title("batch update 1")
        .exec(&db)
        .await
        .unwrap();

    let todo = Todo::get_by_user_id_and_id(&db, &ids[2].0, &ids[2].1)
        .await
        .unwrap();
    assert_eq!(todo.title, "batch update 1");

    // Now fail to update it by scoping by other user
    user2
        .todos()
        .filter_by_id(&ids[2].1)
        .update()
        .title("batch update 2")
        .exec(&db)
        .await
        .unwrap();

    let todo = Todo::get_by_user_id_and_id(&db, &ids[2].0, &ids[2].1)
        .await
        .unwrap();
    assert_eq!(todo.title, "batch update 1");

    let id = user.id.clone();

    // Delete the user and associated TODOs are deleted
    user.delete(&db).await.unwrap();
    assert_err!(User::get_by_id(&db, &id).await);
    assert_err!(Todo::get_by_user_id_and_id(&db, &ids[2].0, &ids[2].1).await);
}

tests!(
    validates_pk_missing_parent,
    validates_pk_missing_primitive,
    crud_user_todos
);
