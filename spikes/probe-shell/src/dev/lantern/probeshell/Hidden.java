package dev.lantern.probeshell;
import java.lang.reflect.*;
final class Hidden {
    static Object call(Object target,String method,Class<?>[] types,Object... args) throws Exception {
        Class<?> c=target instanceof Class ? (Class<?>)target:target.getClass();
        try {
            Method m=c.getMethod(method,types);m.setAccessible(true);
            return m.invoke(target instanceof Class ? null:target,args);
        }catch(ReflectiveOperationException e){
            System.out.println("ERR hidden="+c.getName()+"."+method+" cause="+(e instanceof InvocationTargetException ? e.getCause():e));
            for(Method m:c.getDeclaredMethods())System.out.println("SIGNATURE "+m.toGenericString());
            throw e;
        }
    }
    static Object service(String name) throws Exception {
        return call(Class.forName("com.genymobile.scrcpy.wrappers.ServiceManager"),"get"+name,new Class<?>[]{});
    }
    static Object binder(String name) throws Exception {
        return call(Class.forName(name),"getService",new Class<?>[]{});
    }
}
