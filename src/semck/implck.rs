use std::collections::HashSet;

use crate::ctxt::SemContext;
use dora_parser::error::msg::Msg;
use dora_parser::lexer::position::Position;

pub fn check<'ast>(ctxt: &mut SemContext<'ast>) {
    for ximpl in &ctxt.impls {
        let ximpl = ximpl.read();
        let xtrait = ctxt.traits[ximpl.trait_id()].read();
        let cls = ctxt.classes.idx(ximpl.cls_id());
        let cls = cls.read();
        let cls = cls.ty;

        let all: HashSet<_> = xtrait.methods.iter().cloned().collect();
        let mut defined = HashSet::new();

        for &method_id in &ximpl.methods {
            let method = ctxt.fcts.idx(method_id);
            let mut method = method.write();

            if let Some(fid) = xtrait.find_method(
                ctxt,
                method.is_static,
                method.name,
                Some(cls),
                method.params_without_self(),
            ) {
                method.impl_for = Some(fid);
                defined.insert(fid);
            } else {
                let args = method
                    .params_without_self()
                    .iter()
                    .map(|a| a.name(ctxt))
                    .collect::<Vec<String>>();
                let mtd_name = ctxt.interner.str(method.name).to_string();
                let trait_name = ctxt.interner.str(xtrait.name).to_string();

                let msg = if method.is_static {
                    Msg::StaticMethodNotInTrait(trait_name, mtd_name, args)
                } else {
                    Msg::MethodNotInTrait(trait_name, mtd_name, args)
                };

                report(ctxt, method.pos, msg);
            }
        }

        for &method_id in all.difference(&defined) {
            let method = ctxt.fcts.idx(method_id);
            let method = method.read();

            let args = method
                .params_without_self()
                .iter()
                .map(|a| a.name(ctxt))
                .collect::<Vec<String>>();
            let mtd_name = ctxt.interner.str(method.name).to_string();
            let trait_name = ctxt.interner.str(xtrait.name).to_string();

            let msg = if method.is_static {
                Msg::StaticMethodMissingFromTrait(trait_name, mtd_name, args)
            } else {
                Msg::MethodMissingFromTrait(trait_name, mtd_name, args)
            };

            report(ctxt, ximpl.pos, msg);
        }
    }
}

fn report(ctxt: &SemContext, pos: Position, msg: Msg) {
    ctxt.diag.lock().report_without_path(pos, msg);
}

#[cfg(test)]
mod tests {
    use crate::semck::tests::*;
    use dora_parser::error::msg::Msg;

    #[test]
    fn method_not_in_trait() {
        err(
            "
            trait Foo {}
            class A
            impl Foo for A {
                fun bar() {}
            }",
            pos(5, 17),
            Msg::MethodNotInTrait("Foo".into(), "bar".into(), vec![]),
        );
    }

    #[test]
    fn method_missing_in_impl() {
        err(
            "
            trait Foo {
                fun bar();
            }
            class A
            impl Foo for A {}",
            pos(6, 13),
            Msg::MethodMissingFromTrait("Foo".into(), "bar".into(), vec![]),
        );
    }

    #[test]
    fn method_returning_self() {
        ok("trait Foo {
                fun foo() -> Self;
            }

            class A

            impl Foo for A {
                fun foo() -> A { return A(); }
            }");
    }

    #[test]
    fn static_method_not_in_trait() {
        err(
            "
            trait Foo {}
            class A
            impl Foo for A {
                static fun bar() {}
            }",
            pos(5, 24),
            Msg::StaticMethodNotInTrait("Foo".into(), "bar".into(), vec![]),
        );
    }

    #[test]
    fn static_method_missing_in_impl() {
        err(
            "
            trait Foo {
                static fun bar();
            }
            class A
            impl Foo for A {}",
            pos(6, 13),
            Msg::StaticMethodMissingFromTrait("Foo".into(), "bar".into(), vec![]),
        );
    }
}
